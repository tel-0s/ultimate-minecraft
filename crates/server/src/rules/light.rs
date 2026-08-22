//! Light propagation — partition-aware BFS flood-fill inside the rule.
//!
//! When a `BlockSet` fires, this rule runs a synchronous BFS that
//! recomputes block-light and (if the chunk is sky-lit) sky-light — but
//! only inside the event's HOME chunk. Where the flood would cross a
//! chunk border, it stops and emits a dedup-coalesced `LightNotify` for
//! the first foreign cell instead; that event routes to the neighbor
//! chunk's owner (or cluster-forwards to its node), whose evaluation
//! RE-DERIVES the cell's correct light from current state and continues
//! the flood there, clipped the same way.
//!
//! This closes what used to be the engine's documented ownership
//! exception: the old flood wrote light directly into whatever chunks
//! the radius-14 field reached, racing other workers' floods at borders
//! (and never crossing cluster-node borders at all). Now every chunk's
//! light is written exclusively by its owner, happens-before rides the
//! event transport, and the confluence argument is the fluid/wire one:
//! light has a unique fixpoint given block state, each evaluation moves
//! its chunk toward that fixpoint under current boundary reads, and
//! every boundary write re-notifies the other side — stale reads
//! self-heal, and the settled field is execution-order-independent.
//!
//! Bookkeeping stays cheap: `LightSet`/`LightBatch` are reporting-only
//! (storage is written by the BFS itself), one `LightBatch` per flood,
//! and border crossings coalesce per-cell in the graph.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::block;
use ultimate_engine::causal::event::{Event, EventPayload, LightType};
use ultimate_engine::world::World;
use ultimate_engine::world::block::BlockId;
use ultimate_engine::world::position::{BlockPos, ChunkPos};

const MIN_Y: i64 = -64;
const MAX_Y: i64 = 319;

pub fn light_propagation(world: &World, payload: &EventPayload) -> Vec<Event> {
    match payload {
        EventPayload::BlockSet { pos, old, new } => update_light(world, *pos, *old, *new),
        // A compound rewrite (piston push) changed several cells at once;
        // its synthesized per-cell BlockSets don't evaluate rules, so
        // light folds here. Writes in the home chunk flood locally; a
        // write the chain made in a NEIGHBOR chunk is someone else's
        // light to compute — hand it over as a notify.
        EventPayload::AtomicBlockSet { writes } => {
            let home = writes.first().map(|w| w.pos.chunk());
            let mut events = Vec::new();
            for w in writes.iter() {
                if Some(w.pos.chunk()) == home {
                    events.extend(update_light(world, w.pos, w.old, w.new));
                } else {
                    events.push(Event {
                        payload: EventPayload::LightNotify { pos: w.pos },
                    });
                }
            }
            events
        }
        // Continuation of a flood that crossed into this chunk (or any
        // stale-read heal): re-derive this cell from current block and
        // neighbor light, then flood the difference locally.
        EventPayload::LightNotify { pos } => relight_at(world, *pos),
        _ => Vec::new(),
    }
}

fn update_light(world: &World, pos: BlockPos, old: BlockId, new: BlockId) -> Vec<Event> {
    let old_emit = block::light_emission(old);
    let new_emit = block::light_emission(new);
    let old_opacity = block::light_opacity(old);
    let new_opacity = block::light_opacity(new);

    if old_emit == new_emit && old_opacity == new_opacity {
        return Vec::new();
    }

    let home = pos.chunk();
    let mut events = Vec::new();
    let mut notify: HashSet<BlockPos> = HashSet::new();
    reflow(world, home, pos, new_emit, LightType::Block, &mut events, &mut notify);

    if old_opacity != new_opacity && world.is_sky_lit(&home) {
        let sky_seed = compute_sky_at(world, pos, new_opacity);
        // A newly-opaque cell CUTS its sky column: every direct-column 15
        // below must re-derive from lateral light (level-15 below open
        // sky is only ever column-derived, so clearing exactly the 15s
        // is safe). The old flood never did this — a roof left full
        // daylight trapped beneath it.
        let cut_column = new_opacity > 0;
        reflow_sky(world, home, pos, sky_seed, cut_column, &mut events, &mut notify);
    }

    emit_notifies(notify, &mut events);
    events
}

/// `LightNotify` continuation: bring `pos` (and, transitively, its home
/// chunk) to the light fixpoint given current block state and boundary
/// reads. Handles both kinds — the notify doesn't carry a type, and
/// re-deriving both is cheap.
fn relight_at(world: &World, pos: BlockPos) -> Vec<Event> {
    let home = pos.chunk();
    let mut events = Vec::new();
    let mut notify: HashSet<BlockPos> = HashSet::new();

    let block_desired = compute_block_at(world, pos);
    if block_desired != world.get_block_light(pos) {
        reflow(world, home, pos, block_desired, LightType::Block, &mut events, &mut notify);
    }

    if world.is_sky_lit(&home) {
        let opacity = block::light_opacity(world.get_block(pos));
        let sky_desired = compute_sky_at(world, pos, opacity);
        if sky_desired != world.get_sky_light(pos) {
            // Columns are intra-chunk (chunks span full height), so a
            // cross-border notify is never a column cut.
            reflow_sky(world, home, pos, sky_desired, false, &mut events, &mut notify);
        }
    }

    emit_notifies(notify, &mut events);
    events
}

// ── Cached world access ──────────────────────────────────────────────────────

/// Last-chunk-memoized world access for the BFS hot loops.
///
/// `bench_access` measured the per-read `DashMap` lookup at ~59% of a
/// block read's cost; BFS visits are spatially clustered, so caching the
/// most recent chunk's guard removes most lookups. Holds at most ONE
/// guard at a time — the previous guard is dropped before the next is
/// acquired — required by the codebase-wide lock discipline (single
/// guard, or multiple in canonical order).
struct CachedWorld<'w> {
    world: &'w World,
    current: Option<(
        ChunkPos,
        dashmap::mapref::one::RefMut<'w, ChunkPos, ultimate_engine::world::chunk::Chunk>,
    )>,
}

impl<'w> CachedWorld<'w> {
    fn new(world: &'w World) -> Self {
        Self { world, current: None }
    }

    #[inline]
    fn chunk(&mut self, pos: BlockPos) -> Option<&mut ultimate_engine::world::chunk::Chunk> {
        let cp = pos.chunk();
        let hit = matches!(&self.current, Some((c, _)) if *c == cp);
        if !hit {
            self.current = None; // release the old guard BEFORE acquiring
            self.current = self.world.get_chunk_mut(&cp).map(|r| (cp, r));
        }
        self.current.as_mut().map(|(_, c)| &mut **c)
    }

    #[inline]
    fn get_block(&mut self, pos: BlockPos) -> BlockId {
        self.chunk(pos).map_or(BlockId::AIR, |c| c.get_block(pos.local()))
    }

    #[inline]
    fn get_light(&mut self, pos: BlockPos, ty: LightType) -> u8 {
        match ty {
            LightType::Block => self.chunk(pos).map_or(0, |c| c.get_block_light(pos.local())),
            // Unloaded chunks read as full sky.
            LightType::Sky => self.chunk(pos).map_or(15, |c| c.get_sky_light(pos.local())),
        }
    }

    /// Set light only if the chunk is loaded; returns whether it was.
    #[inline]
    fn set_light_if_loaded(&mut self, pos: BlockPos, ty: LightType, val: u8) -> bool {
        match self.chunk(pos) {
            Some(c) => {
                match ty {
                    LightType::Block => c.set_block_light(pos.local(), val),
                    LightType::Sky => c.set_sky_light(pos.local(), val),
                }
                true
            }
            None => false,
        }
    }
}

// ── The clipped two-phase flood ──────────────────────────────────────────────

/// Two-phase (removal, then re-addition) BFS for one light type, seeded
/// by setting `pos` to `seed`, WRITES CLIPPED to the `home` chunk. Cells
/// the flood would write outside `home` are collected into `notify`
/// instead — their owners re-derive and continue. Foreign cells are
/// still READ (boundary conditions); a stale read is healed by the
/// notify the other side emits when it writes its border.
fn reflow(
    world: &World,
    home: ChunkPos,
    pos: BlockPos,
    seed: u8,
    ty: LightType,
    events: &mut Vec<Event>,
    notify: &mut HashSet<BlockPos>,
) {
    reflow_inner(world, home, pos, seed, ty, false, events, notify);
}

/// Sky-light variant carrying the column-cut flag.
fn reflow_sky(
    world: &World,
    home: ChunkPos,
    pos: BlockPos,
    seed: u8,
    cut_column: bool,
    events: &mut Vec<Event>,
    notify: &mut HashSet<BlockPos>,
) {
    reflow_inner(world, home, pos, seed, LightType::Sky, cut_column, events, notify);
}

#[allow(clippy::too_many_arguments)]
fn reflow_inner(
    world: &World,
    home: ChunkPos,
    pos: BlockPos,
    seed: u8,
    ty: LightType,
    cut_column: bool,
    events: &mut Vec<Event>,
    notify: &mut HashSet<BlockPos>,
) {
    debug_assert_eq!(pos.chunk(), home, "flood seeds in its home chunk");
    let old_level = match ty {
        LightType::Block => world.get_block_light(pos),
        LightType::Sky => world.get_sky_light(pos),
    };

    // Net-change tracker: first observed old value, latest new value per cell.
    let mut changed: HashMap<BlockPos, (u8, u8)> = HashMap::new();
    let mut removal: VecDeque<(BlockPos, u8)> = VecDeque::new();
    let mut addition: VecDeque<BlockPos> = VecDeque::new();

    let mut cw = CachedWorld::new(world);

    if seed != old_level {
        record(&mut changed, pos, old_level, seed);
        cw.set_light_if_loaded(pos, ty, seed);
    }
    if old_level > seed {
        removal.push_back((pos, old_level));
    }
    if seed > 0 {
        addition.push_back(pos);
    }

    // Column cut: clear every direct-column 15 below `pos` into the
    // removal BFS; each re-derives from lateral light. Columns are
    // vertical, so this never leaves the home chunk.
    if cut_column && ty == LightType::Sky && old_level == 15 {
        let mut y = pos.y - 1;
        while y >= MIN_Y {
            let b = BlockPos::new(pos.x, y, pos.z);
            if cw.get_light(b, LightType::Sky) != 15 {
                break;
            }
            record(&mut changed, b, 15, 0);
            cw.set_light_if_loaded(b, LightType::Sky, 0);
            removal.push_back((b, 15));
            y -= 1;
        }
    }

    // Removal phase: clear cells whose level was inherited from the
    // changed region; independent sources get promoted to re-addition.
    while let Some((p, old_l)) = removal.pop_front() {
        for n in p.neighbors() {
            if n.y < MIN_Y || n.y > MAX_Y {
                continue;
            }
            if n.chunk() != home {
                notify.insert(n);
                continue;
            }
            if ty == LightType::Block {
                let n_emit = block::light_emission(cw.get_block(n));
                if n_emit > 0 {
                    addition.push_back(n);
                    continue;
                }
            }
            let n_l = cw.get_light(n, ty);
            if n_l == 0 {
                continue;
            }
            if n_l < old_l {
                record(&mut changed, n, n_l, 0);
                cw.set_light_if_loaded(n, ty, 0);
                removal.push_back((n, n_l));
            } else {
                addition.push_back(n);
            }
        }
    }

    // Addition phase: propagate outward from every live cell in the queue.
    while let Some(p) = addition.pop_front() {
        let p_l = cw.get_light(p, ty);
        if p_l == 0 {
            continue;
        }
        for n in p.neighbors() {
            if n.y < MIN_Y || n.y > MAX_Y {
                continue;
            }
            if n.chunk() != home {
                notify.insert(n);
                continue;
            }
            let n_block = cw.get_block(n);
            let n_opacity = block::light_opacity(n_block);
            let n_current = cw.get_light(n, ty);
            let target = match ty {
                LightType::Block => {
                    let n_emit = block::light_emission(n_block);
                    p_l.saturating_sub(1.max(n_opacity)).max(n_emit)
                }
                LightType::Sky => {
                    // Column rule: level 15 moving straight down through a
                    // transparent cell keeps 15 (no attenuation).
                    if p_l == 15 && n.y == p.y - 1 && n_opacity == 0 {
                        15
                    } else {
                        p_l.saturating_sub(1.max(n_opacity))
                    }
                }
            };
            if target > n_current {
                record(&mut changed, n, n_current, target);
                if !cw.set_light_if_loaded(n, ty, target) {
                    continue;
                }
                addition.push_back(n);
            }
        }
    }

    drop(cw);
    emit_light_events(changed, ty, events);
}

// ── Fixpoint re-derivation (LightNotify continuations) ───────────────────────

/// The block-light value `pos` SHOULD have: its own emission, or the best
/// neighbor contribution through its opacity.
fn compute_block_at(world: &World, pos: BlockPos) -> u8 {
    let id = world.get_block(pos);
    let emit = block::light_emission(id);
    let opacity = block::light_opacity(id);
    let best_nb = pos
        .neighbors()
        .into_iter()
        .filter(|nb| nb.y >= MIN_Y && nb.y <= MAX_Y)
        .map(|nb| world.get_block_light(nb))
        .max()
        .unwrap_or(0);
    best_nb.saturating_sub(1.max(opacity)).max(emit)
}

/// Compute what sky-light should be at `pos` given its opacity, honoring the
/// direct-column rule for transparent cells under an unobstructed sky.
fn compute_sky_at(world: &World, pos: BlockPos, opacity: u8) -> u8 {
    if opacity == 0 {
        let above = BlockPos::new(pos.x, pos.y + 1, pos.z);
        if above.y <= MAX_Y && world.get_sky_light(above) == 15 {
            let above_opacity = block::light_opacity(world.get_block(above));
            if above_opacity == 0 {
                return 15;
            }
        }
    }
    let best_nb = pos
        .neighbors()
        .into_iter()
        .filter(|nb| nb.y >= MIN_Y && nb.y <= MAX_Y)
        .map(|nb| world.get_sky_light(nb))
        .max()
        .unwrap_or(0);
    best_nb.saturating_sub(1.max(opacity))
}

// ── Shared helpers ───────────────────────────────────────────────────────────

fn record(changed: &mut HashMap<BlockPos, (u8, u8)>, pos: BlockPos, old: u8, new: u8) {
    changed
        .entry(pos)
        .and_modify(|e| e.1 = new)
        .or_insert((old, new));
}

fn emit_notifies(notify: HashSet<BlockPos>, events: &mut Vec<Event>) {
    for pos in notify {
        events.push(Event { payload: EventPayload::LightNotify { pos } });
    }
}

fn emit_light_events(
    changed: HashMap<BlockPos, (u8, u8)>,
    light_type: LightType,
    events: &mut Vec<Event>,
) {
    // ONE LightBatch event per flood instead of one LightSet per cell:
    // these are reporting-only (the BFS already wrote light storage), and
    // per-cell events made graph bookkeeping ~95% of a torch placement's
    // cost (~1,800 inserts/executes/reaps for zero physics).
    let cells: Vec<ultimate_engine::causal::event::LightCell> = changed
        .into_iter()
        .filter(|(_, (old_v, new_v))| old_v != new_v)
        .map(|(pos, (old, new))| ultimate_engine::causal::event::LightCell {
            pos,
            light_type,
            old,
            new,
        })
        .collect();
    if !cells.is_empty() {
        events.push(Event {
            payload: EventPayload::LightBatch { changes: cells.into() },
        });
    }
}
