# Phase 5 — Entities as Causal Actors (design)

> Blocks are stationary causality: events happen *at* positions. Entities are
> mobile causality: the position itself is state, and it changes continuously.
> This design makes continuous motion compatible with a tick-less engine, and
> makes an entity that nothing is happening to cost **zero** — the moving
> sibling of "an idle world burns no CPU."

## 1. The core abstraction: parametric state + events only at causally-relevant times

A tick-based engine integrates every entity every 50 ms. We refuse the tick, so
we need motion without integration. The answer is the same trick vanilla
*clients* already use to render smoothly at 60 fps from 20 Hz updates —
extrapolation — promoted to the server's source of truth:

**An entity's state is a parametric trajectory, not a position.**

```rust
EntityState {
    kind: EntityKind(u16),      // opaque to the engine, like BlockId
    pos:  Vec3,                 // position at `stamp`
    vel:  Vec3,                 // velocity at `stamp`
    stamp: Nanos,               // engine-clock time this state was true
    aux:  u64,                  // opaque game data (e.g. block id for FallingBlock)
}
```

`position(t) = pos + vel·(t − stamp) + ½·g·(t − stamp)²` is a *pure function*.
Between events nothing is computed, stored, or scheduled per-frame. The engine
only executes an event for an entity at the next **causally-relevant time**:

- **t_hit** — first collision of the swept trajectory with the block lattice,
- **t_edge** — trajectory crosses a chunk/region boundary (ownership handoff),
- **t_cap** — a maximum extrapolation horizon (bounds client drift; ~1 s),
- **t_timer** — game timers (item despawn, burn-out, AI think),
- **now** — an external cause fires (`EntityWake`: a block changed under it,
  a player reached for it, another entity's event touched it).

A falling item is ~2 events (launch, land) instead of 20/sec. An item at rest
is **0 events** until the world changes under it. 100k ballistic entities ≈
200k total events — vanilla ticks 160k sand entities at 3.2M integrations/sec
and stays alive only by rationing everything else. This is the scalability
headline of Phase 5.

## 2. Engine additions (`crates/engine`)

Deliberately mirrors the block design: the engine knows entities *exist*, have
identity, kinematic state, and spatial location. It does not know what an item
or a zombie is (that's `crates/server`, like `BlockType` vs `BlockId`).

### 2.1 Storage — `world::entity`

```rust
pub struct EntityId(u64);                    // NOT slotmap: stable across nodes/serialization
pub struct EntityStore {
    entities: DashMap<EntityId, EntityState>,
    by_chunk: DashMap<ChunkPos, HashSet<EntityId>>,   // spatial index, kept in sync on writes
    next_id: AtomicU64,                                // node-salted to avoid cross-node collisions
}
impl World {
    pub fn entities(&self) -> &EntityStore;            // World gains entities beside chunks
}
```

`EntityStore` write API mirrors blocks: `set_entity(id, old, new)` applies the
same **first-write-wins stale guard** as `BlockSet` (compare current vs `old`),
maintains `by_chunk`, and is `&self` (DashMap interior mutability). `by_chunk`
is the index for: AOI queries (which entities does a joining/moving player
see), wake-on-block-change, and interaction candidate lookup.

Because `RuleFn = fn(&World, &EventPayload) -> Vec<Event>` and entities hang
off `World`, **the rule signature does not change**. Entity rules are ordinary
rules.

### 2.2 Events — two new payloads

```rust
EventPayload::EntitySet {
    id: EntityId,
    old: Option<EntityState>,   // None = spawn
    new: Option<EntityState>,   // None = despawn
},
EventPayload::EntityWake {
    id: EntityId,
    at: BlockPos,               // routing hint: entity's chunk at emission
},
```

- `EntitySet` is the entity analog of `BlockSet`: value-carrying, stale-guarded
  (applies only if store state == `old`), **write-logged** — which means the
  entire 6f machinery works untouched: `WriteSync` mirrors entity state to
  every replica, gateways serve entities from replicas, region **migration
  stays a pure ownership flip** (replicas are already entity-current), and
  N-node quiescence counts entity events like any other.
- `EntityWake` is the entity analog of `BlockNotify`: idempotent,
  dedup-coalesced (`DedupKey::EntityWake(id)`), meaning "re-evaluate this
  entity now." Spurious wakes are cheap and safe.
- `Event::chunk()` routes `EntitySet` by `new.pos` (or `old.pos` for despawn)
  and `EntityWake` by `at` — entity events partition/forward/cluster-route
  exactly like block events, through `step_routed` unchanged.

### 2.3 Timed events — the one genuinely new engine mechanism

> **Implementation refinement (2026-07-01):** the delay is a payload
> *wrapper*, not an `Event` field — `EventPayload::After { at, inner }`.
> Adding a field to `Event` would have touched all 107 construction sites
> and the cluster frame format; a wrapper touches neither, serializes as
> just another payload variant, and routes by `inner.chunk()` so timed
> events reach their owner like any other event.

```rust
EventPayload::After {
    at: Nanos,                  // not-before deadline, engine time
    inner: Box<EventPayload>,   // executes as a root when due
}
```

- **The causal graph stays pure-causal.** Timers live in the *physics worker*:
  a `BinaryHeap<(Nanos, Event, u8 /*prio*/)>` per worker. The `step_routed`
  router checks `not_before`: due → insert into the graph as before; future →
  timer heap. The worker's `recv` uses a timeout of `min(next deadline)`; on
  expiry the event inserts as a root (its cause already executed — the delay
  *is* the happens-before edge riding wall-clock instead of a channel).
- **Clock abstraction**: `trait Clock { fn now(&self) -> Nanos }` injected via
  `PhysicsOptions` (default: monotonic `Instant`-based). Tests inject a
  `ManualClock` and step virtual time — determinism and causal-invariance
  tests for entities stay exact, and benches can fast-forward a 5-minute
  despawn in zero wall time.
- **Quiescence**: `pending()` splits into `pending_now` and `pending_timed`.
  Benches/tests quiesce on `pending_now == 0` with an explicit horizon for
  timed events (`run_until(clock, t)`); the cluster `Pong` carries both
  counters. A resting item with a despawn timer must not hold the world
  "non-quiet" forever.
- Cluster codec: `Forward` frames gain the optional `not_before` (u64 nanos,
  0 = none). Deadlines are relative to the engine epoch exchanged in `Hello`.

## 3. Server additions (`crates/server`)

### 3.1 The kinematics rule — where trajectories are computed

A rule on `EntityWake` / `EntitySet` (registered per entity-kind family):

1. Read current `EntityState` from the store (stale events no-op via guard).
2. Integrate the game's kinematics for this kind (gravity, drag, bounce).
3. **Swept-AABB against the block lattice** → `t_hit` (uses the same
   `CachedWorld` chunk-memo pattern as the light BFS; a trajectory rarely
   leaves 1–2 chunks between events).
4. `Δ = min(t_hit, t_edge, t_cap)` → emit ONE consequent
   `EntitySet { old: current, new: state_at(Δ), not_before: stamp + Δ }`.
5. **At rest** (supported && |vel| < ε): emit *nothing*. The entity sleeps.
   Rest-state invariant: a resting entity is woken exclusively by
   `EntityWake` (block change / interaction / timer).

### 3.2 Wake-on-block-change

The existing block rules gain one emission: when `BlockSet` executes, consult
`world.entities().by_chunk` for the affected chunk and emit `EntityWake` for
entities whose **support or path could be affected** (MVP: any entity in the
chunk whose AABB-column intersects the changed cell ± 1; refinement later:
per-cell trajectory registration). Waking a resting entity that's still
supported re-runs step 1–5 and re-sleeps — cheap, idempotent, and exactly the
self-stabilization pattern the fluid rules already use against stale reads.

### 3.3 Entity–entity interaction

Interactions are events, ordered by the owner's graph:

- When an entity event executes, the rule queries `by_chunk` for candidates in
  range (pickup radius, collision box). For each interaction it emits a
  guarded consequent — e.g. `ItemPickup { item, player }` → routed to the
  *item's* owner (its chunk), which despawns via
  `EntitySet { old: Some(state), new: None }`.
- Cross-owner interactions ride `Forward` like any cross-region consequent;
  the stale guard arbitrates races (two players lunging for one item: first
  `EntitySet{ → None }` wins, the loser's guard fails, no dupe). **Same
  confluence discipline as fluids: interactions must be guarded + idempotent.**

### 3.4 Players are entities (bridged, then unified)

MVP: `PlayerRegistry` stays authoritative for connections, but each player is
*mirrored* into the `EntityStore` (kind = Player) on join/move, so entity
rules can see players (pickup proximity) with no special cases. Phase 5
end-state: the mirror inverts — `EntityStore` becomes authoritative, the
registry becomes a connection⇄entity map. (This eventually replaces the
`entity_spawn_cap` hack with true AOI, §3.5.)

### 3.5 Client projection — AOI entity lifecycle

- `event_bus::collect_entity_changes(&[EventPayload])` turns write-logged
  `EntitySet`s into `SpatialMsg::Entity { id, kind, spawn/move/despawn,
  pos, vel }`, published **per-region** (already O(nearby)).
- Connections keep `spawned_entities: HashSet` (exists since the presence-cap
  work) as the client's ground truth: on region **subscribe** (view change),
  query `by_chunk` for the region's entities → spawn packets; on
  **unsubscribe** → despawn packets; `SpatialMsg::Entity` for a known id →
  motion packet (MC `SetEntityMotion` + `TeleportEntity`; clients extrapolate
  natively — our parametric state maps 1:1 onto the vanilla wire model).
- Move coalescing (keep latest per entity per drain) already exists and
  applies unchanged.

## 4. Walkthrough: a dropped item's life

1. Player breaks a block → break cascade → item rule emits
   `EntitySet { old: None, new: item @ (pos, vel↑, stamp=now) }` +
   a despawn `EntityWake` with `not_before = now + 5 min`.
2. Kinematics rule runs on the spawn: swept-AABB says floor impact in 0.62 s →
   emits `EntitySet { new: state_at_impact, not_before: +0.62 s }`.
   Clients in the region got spawn + velocity; they animate the arc locally.
3. 0.62 s later the worker's timer fires, the event inserts, executes, entity
   comes to rest → **no consequent**. Zero cost from here.
4. A player walks near → their move event's rule sees the item in `by_chunk`
   → emits guarded pickup → item's owner despawns it → `EntitySet → None`
   write-logs → replicas + clients see it vanish. (The 5-min despawn wake
   later finds no entity — stale, no-op.)
5. Or: someone breaks the floor under it → `BlockSet` emits `EntityWake` →
   kinematics finds no support → new ballistic segment. The item falls.

Region/node handoff (the case that proves mobility): if step 2's trajectory
crosses a region boundary before impact, `t_edge < t_hit`, so the emitted
`EntitySet` lands at the boundary — and `step_routed` sees its chunk belongs
to another worker/node and **forwards it**. The entity's next event simply
executes on its new owner; `WriteSync` had already primed every replica.
Ownership follows the entity with no new protocol.

## 5. Invariants (the contract entity rules must honor)

1. **Guarded writes**: every `EntitySet` carries `old`; first-write-wins.
2. **Idempotent wakes**: `EntityWake` must be safe to deliver spuriously,
   duplicated, or late (it re-reads current state and re-derives).
3. **Rest costs nothing**: a supported, still entity emits no consequents.
4. **One in-flight segment per entity**: the kinematics rule emits exactly one
   future `EntitySet`; a wake that changes the trajectory makes the old
   in-flight event stale (its `old` no longer matches) — superseded segments
   die at the guard, never fork.
5. **Cross-owner effects ride events**, never direct writes (light BFS remains
   the sole documented exception).

## 6. MVP cut (next session) and follow-ons

**MVP: dropped-item entity** — fully additive (touches no existing rule
semantics), exercises every new mechanism: spawn on block break, ballistic
trajectory, landing, rest-sleep, wake-on-block-change, despawn timer (timed
events at long horizon), AOI spawn/despawn on clients, pickup-despawn on
player proximity (interaction + guard).

Then, in order:
- **FallingBlock entity** (vanilla sand/gravel parity), gated by a rules
  option so the crown-jewel benches keep their instant-gravity workload.
- **Bench**: 100k ballistic items vs the measured vanilla 160k-sand numbers.
- **Players unified into EntityStore**; `entity_spawn_cap` → true AOI.
- **Mob skeleton**: AI "think" = timed `EntityWake` self-chain (priority 0;
  player-adjacent thinks inherit priority 1) — AI cadence becomes a per-mob
  *rate*, not a global tick.

## 7. Test & bench plan

- `ManualClock` unit tests: trajectory event at exact deadline; despawn at
  horizon; cap segmentation (t_cap chains).
- Causal invariance: same drops, shuffled execution orders, N workers ∈
  {1, 4, 16} → identical final entity states (mirrors `bench_partitioned`'s
  cross-worker world equality; add entity-state equality to that assert).
- Contention: two players, one item, all interleavings → exactly one pickup.
- Cluster: item thrown across a node boundary lands identically on all
  replicas (extends `tests/cluster.rs` mixed workload).
- Handoff under migration: migrate a region while an entity's timed event is
  in the old owner's heap → wake executes on… (**open question 2**).

## 8. Open questions (status after the MVP, 2026-07-01)

1. **Timer heap vs graph timers — RESOLVED, better than planned**: no
   drain-on-`Transfer` needed at all. A parked timer is just an event; when
   it fires, it re-enters the NORMAL router, which reads the *current*
   assignment/ownership tables — so a migrated region's timers forward
   themselves to the new owner at fire time. Ownership is consulted at
   execution, never captured at parking.
2. **Wake precision**: chunk-granular wake-on-block-change over-wakes (any
   block change wakes all entities in the chunk column ±1.5 blocks
   horizontally). Fine at MVP scale; the refinement (per-cell trajectory
   registration) only matters with dense entities + heavy digging.
3. **`EntityState.aux` sizing**: u64 suffices (items pack despawn-deadline
   ms in the high 48 bits + block id in the low 16). Mobs will need a real
   component blob — revisit when the mob skeleton lands.
4. **Engine epoch across nodes**: `After.at` compares against local
   monotonic clocks; mesh nodes should exchange epoch offsets in `Hello`
   (not yet wired — single-node timers are exact, cross-node timed
   forwards land *at-or-after*, never *exactly-at*, which the confluence
   discipline already tolerates).
5. **Exactly-once item drops** (new, solved during MVP): a dropped item
   must spawn iff the break's write took effect. Rule-level spawning can't
   distinguish a player break from rule-driven block movement (sand leaving
   a cell is also solid→air), so the drop triggers on the action's
   `BlockSet` appearing in the WRITE LOG — the log only ever contains the
   one effective write for a contested cell. `BlockAction.drop_item` +
   per-batch drop-watch in the worker.
6. **Conversion atomicity** (new, solved for FallingBlock at 160k scale):
   entity→block conversion spans two stores, so it CANNOT be one event
   pair — every combination of (despawn, block-write) emitted together
   loses or duplicates sand under contention. The stable shape:
   **materialize first, despawn as a consequent of the effective block
   write** (the write-log/rule-eval chain is the atomicity primitive),
   with blocked landings BUMPING one cell up as immediate-lane rest
   states. Sand is then conserved on every path — verified exactly at
   160k with 16-deep co-column stacking.
7. **OPEN — conversion vs region-handoff dual ownership**: the 6d
   rebalancer's transient dual-ownership window can reorder a landing's
   despawn against a wake-bump across two workers (~0.2% duplication at
   160k with rebalancing on). Block rules tolerate dual windows by
   confluence; cross-store conversions don't yet. Options: quiesce-region
   handoff for entity-resident regions, or make conversion single-event
   (engine-level combined payload). Decide with players-in-EntityStore.

## 9. MVP results (2026-07-01)

Implemented: engine entity store + payloads + timer plane + `ManualClock`;
item kinematics/spawn/despawn rules; spatial + cluster + client projection
(AddEntity/metadata/teleport/remove, view backfill, pickup with collect
animation). 188/188 tests green, including the new `tests/entities.rs`:
full lifecycle in virtual time, **a resting item executes exactly zero
events across 60 virtual seconds**, trajectories bit-identical across
worker counts, contested breaks and pickups exactly-once.
