# Ultimate Minecraft -- Roadmap

A **real Minecraft 1.21.11 server** built on **causal graph dynamics**: no global tick
clock, maximal parallelism from causal independence, Wolfram-inspired local rewriting
rules on a sparse 3D block lattice. Real MC clients connect, see a world, and walk around.

> *Time is not a global parameter but the depth of the causal graph.*

---

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│  Minecraft 1.21.11 Client (vanilla, Fabric, etc.)       │
└───────────────────────────┬─────────────────────────────┘
                            │ MC Protocol (774)
┌───────────────────────────┴─────────────────────────────┐
│  ultimate-server                                         │
│  ┌──────────────┐ ┌────────────┐ ┌───────────────────┐  │
│  │ net/         │ │ block.rs   │ │ rules/            │  │
│  │  listener    │ │ BlockType  │ │  gravity          │  │
│  │  connection  │ │ properties │ │  fluid_spread     │  │
│  │  (azalea)    │ │            │ │  (causal rules)   │  │
│  └──────┬───────┘ └─────┬──────┘ └────────┬──────────┘  │
│         │               │                  │             │
│  ┌──────┴───────────────┴──────────────────┴──────────┐  │
│  │  ultimate-engine (game-agnostic)                    │  │
│  │  ┌─────────┐ ┌──────────────┐ ┌─────────────────┐  │  │
│  │  │ World   │ │ CausalGraph  │ │ Scheduler       │  │  │
│  │  │ Chunk   │ │ Event DAG    │ │ seq + parallel  │  │  │
│  │  │ BlockId │ │ Frontier     │ │ (rayon)         │  │  │
│  │  └─────────┘ └──────────────┘ └─────────────────┘  │  │
│  └────────────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────────────┘
```

The world is a sparse 3D lattice of blocks stored in chunks. Instead of a global
20 TPS tick loop, the server maintains a **causal graph** of local update events.
Events with no causal dependency are **spacelike-separated** and execute in parallel.
Clients are projected a coarse-grained view at their connection rate, but internally
causality is the only ordering.

---

## Phase 0 -- Foundation ✓

> Compilable skeleton with core types, working demo.

- [x] Cargo workspace: `crates/engine/` + `crates/server/`
- [x] Engine: `BlockId(u16)`, `Chunk`, `World`, `CausalGraph`, `Scheduler`
- [x] Server: block types, gravity/fluid rules, sand-drop demo (94 events)

## Phase 1 -- Causal Engine ✓

> Prove correctness and causal invariance.

- [x] 19 tests: graph unit tests, sand/water rules, 3 causal invariance tests,
      3 parallel-vs-sequential equivalence tests
- [x] DOT export for Graphviz visualization
- [x] Causal invariance proven: world state identical across all frontier orderings

## Phase 2 -- Parallel Execution ✓

> Spacelike-separated events execute concurrently.

- [x] Snapshot-scatter-gather parallel scheduler via `rayon::par_iter`
- [x] Chunk-level spatial partitioning for independence detection
- [x] Benchmark: 385K events, worlds verified identical (seq ≈ par)
- [ ] Incremental frontier tracking (see Phase 6a)
- [ ] Event deduplication (see Phase 6a)

## Phase 3 -- Networking & Players (current)

> Real Minecraft 1.21.11 clients connect and play.

### Done ✓
- [x] `azalea-protocol` 0.15 integration (MC 1.21.11, protocol 774)
- [x] Rust nightly toolchain (required by `simdnbt`)
- [x] TCP server with async I/O (`tokio`)
- [x] Protocol state machine: Handshake -> Status -> Login -> Configuration -> Play
- [x] Server list ping (shows in MC multiplayer menu)
- [x] Offline-mode login (no encryption, no compression)
- [x] Known Packs registry exchange (13 registries, all entry IDs from azalea-registry)
- [x] UpdateTags for timeline registry
- [x] Chunk serialization: 1.21.5+ format (Prefixed Array heightmaps, no VarInt data length)
- [x] Player spawn with teleport confirmation
- [x] Game Event 13 (start waiting for chunks)
- [x] Keep-alive loop
- [x] **CLIENT CONNECTS AND SEES A FLAT WORLD** ✓

- [x] Block break -> causal event -> BlockUpdate packet
- [x] Block place (stone) -> causal event -> BlockUpdate packet
- [x] BlockChangedAck sequence confirmation
- [x] Resilient packet parsing (modded client extra data doesn't crash)

### Next
- [x] Persistent world: pre-populate World at startup, serve chunks from World state
- [x] Use MC block state IDs as BlockId values (unify engine + protocol ID space)
- [x] Creative inventory: place the block the player is holding, not always stone
- [x] Multiple simultaneous players (each sees the other's changes)
      -- `event_bus` broadcast + `player_registry` cross-player join/move/leave/chat
- [x] Chunk loading based on player position (send new chunks as player moves)
      -- distance-sorted view_distance=4, reloads on chunk boundary cross
- [x] Chat messages
      -- `ClientboundSystemChat` out, `PlayerEvent::Chat` through registry

### Phase 3 addenda (landed beyond the original list)
- [x] Pluggable ambient simulation framework (`simulation.rs`), one tokio task per layer
- [x] Vanilla-accurate block placement orientation (`placement.rs`: facing/axis/half/rotation/stair shape)
- [x] Light engine with emission + opacity + sky-light column propagation
- [x] Dashboard (graph snapshots + metrics)
- [x] **Chunk render fix**: work around azalea-core's `ChunkPos` u64 packing
      bug for negative coordinates, where `(x as u64) | ((z as u64) << 32)`
      sign-extends a negative i32 across all 64 bits and silently destroys z.
      `send_forget_level_chunk` builds the packet manually with `((cx as u32) as u64)`.
- [x] **MC 1.20+ chunk batching**: wrap chunk sends in
      `ChunkBatchStart` / `ChunkBatchFinished` markers — the client otherwise
      receives the data but holds the chunks in a "pending batch" state and
      won't render them.

## Phase 4 -- World Generation

> Infinite procedural terrain, lazily generated. Replicating vanilla 1.21
> overworld is multi-week work; broken into staged sub-phases.

### 4a -- Heightmap terrain (compositional, JSON-driven)
- [x] `WorldGen` trait + on-demand chunk generation hook in chunk-loading paths
- [x] **Density-function framework** (`worldgen::density`): composable scalar
      fields over `(x,y,z)`. Atoms: `constant`, `y_index`, `noise2d`, `noise3d`.
      Combinators: `add`, `sub`, `mul`, `min`, `max`, `clamp`. Mirrors
      vanilla 1.18+'s noise-router shape; tree described in JSON, compiled
      to `Arc<dyn DensityFunction>` at startup.
- [x] **Pipelines** (`worldgen::pipeline`): `DensityPipeline` walks each
      column top-down through the density function, stratifying
      bedrock / stone / dirt / grass / sand / water + sea level. `FlatPipeline`
      for superflat (bedrock + layer stack).
- [x] **JSON presets** (`worldgen::preset`): built-in `"noise"` and
      `"superflat"`, or any path to a JSON file. Schema uses
      `#[serde(tag = "kind")]` for preset kind and `#[serde(tag = "type")]`
      for density-function nodes — fully data-driven, no recompile needed
      to swap pipelines.
- [x] Operator-configurable via `world.preset` in `server.yaml`.
- [x] Deterministic from `world.seed` (CLI `--seed` overrides).
- [x] Pre-generate spawn area at startup; further chunks generated lazily.

### 4b -- Multi-noise biome system
- [ ] Continentalness, erosion, peaks-and-valleys noise fields
- [ ] Climate noise (temperature, humidity)
- [ ] Biome assignment from multi-noise (plains, mountains, desert, forest, ocean, beach, river, snowy, etc.)
- [ ] Per-biome surface rules (grass / sand / snow / podzol)
- [ ] Per-biome height profiles (flat plains vs peaked mountains)

### 4c -- Caves & ores
- [ ] 3D-noise / worley cave carving (cheese caves + spaghetti tunnels)
- [ ] Aquifers (water-filled cave regions)
- [ ] Ore distribution by depth (coal / iron / diamond bands)

### 4d -- Decorators & structures
- [ ] Trees (oak, birch, jungle, spruce, dark oak, acacia, mangrove, cherry)
- [ ] Plants (flowers, grass, kelp, sugarcane, etc.)
- [ ] Simple structures (villages, dungeons)
- [ ] Decorator framework: per-biome populator with rule-based placement

## Phase 5 -- Entities & Physics

> Moving objects integrated into the causal graph.

- [ ] Entity as a causal actor: position updates are events with spatial dependencies
- [ ] AABB collision detection against block grid
- [ ] Gravity, jumping, basic kinematics
- [ ] Entity-entity interaction as causally-ordered events
- [ ] Mob spawning rules, basic mob AI

## Phase 6 -- Scale & Optimization

> Push toward the theoretical limits.
>
> **Performance thesis:** With the bottlenecks below resolved, the block physics
> subsystem should sustain ~500M events/sec/core (~3-4 billion events/sec on an
> 8-core desktop). Causal propagation velocity: ~10-30 million blocks/sec/core,
> roughly a million-fold speedup over vanilla's 20-blocks/sec tick-based propagation.
> The ceiling shifts from CPU to memory capacity and feature complexity.

### 6a -- Algorithmic (highest priority, largest impact)

These are O(N)-to-O(1) improvements that prevent performance from degrading over time.

- [x] **Incremental frontier tracking**
      `CausalGraph` maintains a `ready: VecDeque<EventId>` updated on insert and
      `mark_executed`. `drain_ready()` is amortized O(1) per event. The old
      `frontier()` full-scan is kept for tests/debugging.

- [ ] **Causal graph pruning / garbage collection**
      Once an event is executed and all its children are also executed, the node can be
      reaped from the `SlotMap`. Maintain a reference count or check at execution time:
      when marking a node executed, walk its parents -- if a parent is executed and all
      its children are now executed, remove the parent. Keeps the graph bounded to the
      active wavefront rather than growing without limit.
      *Current cost: unbounded memory growth, proportional to total lifetime event count.*

- [x] **Event deduplication / coalescing**
      `CausalGraph::insert` transparently coalesces idempotent events
      (`BlockNotify`, `LightNotify`) against any pending event sharing the
      same `EventPayload::dedup_key()`. Parents are merged into the existing
      event; no new node is created. `drain_ready` re-checks parents at pop
      time so merges that add unfinished parents correctly delay firing.
      Non-idempotent events (`BlockSet`, `LightSet`) whose identity depends
      on their value fields are never coalesced.

- [x] **Light engine: BFS-inside-rule** *(not originally listed — added after torch
      cascades halted the server)*
      `rules/light.rs` was rewritten from an event-cascading model to a synchronous
      BFS flood-fill inside a single rule invocation. Two-phase classic algorithm
      (removal then re-propagation) for both block-light and sky-light, honoring
      the vanilla sky-column special case. `LightSet` events are emitted per
      changed cell for reporting via `event_bus::collect_light_changes`, but
      never produce consequent events. Collapses ~100K events per torch to ~11K
      events of pure bookkeeping cost.

- [ ] **Idempotent rule guards**
      Rules currently re-evaluate blocks that have already been handled. Add a check
      in gravity/fluid rules: if the event's `old` state matches the current world
      state (meaning another event already moved the block), skip. This is complementary
      to deduplication and prevents redundant cascades at the rule level.

### 6b -- Concurrency model (unlock true multi-core scaling)

Replace the current per-connection isolated graphs and DashMap locking with a
shared-nothing spatial ownership model.

- [ ] **Shared causal graph**
      Replace per-connection `CausalGraph` with a single shared graph. All player
      actions feed into one DAG so cross-player causality is tracked (player A breaks
      a block supporting player B's sand). Requires concurrent graph insertion --
      either a lock-free append-only arena or a dedicated graph-owner thread with
      a channel-based interface.

- [ ] **Chunk ownership partitioning**
      Replace `DashMap<ChunkPos, Chunk>` with a static spatial partitioning scheme:
      assign each chunk to a thread (or core) deterministically. Intra-partition
      events require zero synchronization -- the owning thread has exclusive access.
      Cross-boundary reads (e.g., gravity rule checking a block in an adjacent chunk
      that belongs to a different partition) use lock-free snapshots or message passing.
      ~90-95% of events are intra-chunk, so synchronization cost is amortized to
      near zero.

- [ ] **Decoupled physics from connection handler**
      Currently `run_until_quiet()` runs synchronously inside the tokio task that
      handles packets, blocking that player's I/O during cascades. Decouple: the
      connection handler inserts root events into a shared submission queue, and a
      dedicated physics thread pool drains the causal frontier continuously. Completed
      events are broadcast to relevant connections via a channel.

- [ ] **Batch event submission**
      Accumulate root events from all players over a short window (~100us-1ms) before
      draining the frontier. This increases the frontier width (more spacelike-separated
      events available per step), improving parallel utilization. The batch window is
      not a tick -- it's a parallelism optimization with no semantic commitment to a
      fixed rate.

### 6c -- Memory & allocation (reduce per-event overhead, improve cache behavior)

- [ ] **Locality-aware memory allocation**
      Allocate chunk data and causal graph nodes in spatial-locality-preserving arenas.
      Chunks that are spatially adjacent should be adjacent in memory so that cross-chunk
      rule evaluation (reading a neighbor block across a chunk boundary) hits L1/L2
      cache rather than causing a cache miss. Use a space-filling curve (Morton/Z-order
      or Hilbert curve) to map `ChunkPos` to arena offsets. On NUMA systems, pin each
      spatial partition's arena to the memory node local to the core that owns it.

- [ ] **Arena allocation for events**
      Replace per-rule `Vec<Event>` heap allocations with a bump allocator or object
      pool. Each scheduler step allocates a fresh arena; all consequent events are
      bump-allocated into it. The arena is freed in bulk after the gather phase.
      Eliminates thousands of small allocations per step.

- [ ] **SlotMap generational arena tuning**
      Configure `SlotMap` with a custom page size matched to L1 cache line boundaries
      (64 bytes). Ensure `EventNode` is sized/aligned to avoid straddling cache lines.
      Consider a `DenseSlotMap` variant for better iteration performance during
      frontier scans (if incremental frontier isn't yet implemented) or graph pruning
      sweeps.

- [ ] **Delta-encoded chunk storage**
      Store chunks as deltas from a procedurally generated baseline. For worlds with
      a terrain generator, most blocks never change from their generated state. Only
      store the diff. Reduces memory footprint by 10-100x for natural terrain, allowing
      far more chunks to be loaded simultaneously.

- [ ] **Compact block state representation**
      Investigate palette-based sections (like MC's own wire format) as the runtime
      representation, not just the serialization format. For sections with few unique
      block types (common), a 4-bit palette index halves memory vs. raw `u16` BlockId,
      doubling the number of sections that fit in cache.

### 6d -- Scheduling & work distribution

- [ ] **Adaptive region sizing**
      Dynamically adjust spatial partitions based on event density. Dense areas (many
      players, active redstone, flowing water) get smaller regions with dedicated cores.
      Sparse areas are merged into larger regions handled by a single core. Rebalance
      periodically based on event count per partition.

- [ ] **Priority-aware frontier draining**
      Not all events are equal. Player-initiated events and their immediate cascades
      should be prioritized over background physics (distant water spreading, etc.)
      to minimize perceived latency. Use a priority queue for the frontier, with
      priority based on causal distance from a player action.

- [ ] **Work-stealing across partitions**
      When a core's partition is quiescent (no pending events), it should steal work
      from a neighboring partition. Use rayon's existing work-stealing pool but with
      spatial-affinity hints so stolen work is likely to be cache-warm (prefer stealing
      from adjacent partitions whose chunk data may be in shared L2/L3).

### 6e -- Measurement & validation

- [ ] Load testing: 1k, 10k, 100k simulated players
- [ ] Microbenchmarks: events/sec/core, quiescence latency by cascade type
- [ ] Causal propagation velocity metric (blocks/sec) as a first-class benchmark
- [ ] Formal causal invariance proof for full rule set
- [ ] Comparison metrics vs. traditional tick-based architecture (vanilla, Paper, Folia)
- [ ] Memory profiling: per-player footprint, graph growth rate, arena utilization
- [ ] Flame graphs for event processing hot path
- [ ] Cross-player causality integration tests

### 6f -- Distribution (future, when single-machine limits are reached)

- [ ] Distributed execution across machines (partition spatial regions across nodes)
- [ ] Cross-node causal ordering protocol (vector clocks or Lamport timestamps)
- [ ] Region migration for load balancing
- [ ] Edge-node architecture: physics nodes + gateway nodes for client connections

---

## Design Principles

1. **Causal invariance over global synchronization.**
   Two updates that don't causally depend on each other must produce the same
   result regardless of execution order. This is the source of all parallelism.

2. **Sparse over dense.**
   Only store and compute what exists. Empty chunks cost nothing.

3. **Local rules, global behavior.**
   Every rule reads and writes only a bounded neighborhood.

4. **Client time is an illusion.**
   The server has no tick clock. Clients receive a projected, coarse-grained
   view at whatever rate their connection supports.

5. **Measure everything.**
   Every phase includes benchmarks.

---

## Tech Stack

| Component       | Choice                  | Why                                            |
|-----------------|-------------------------|------------------------------------------------|
| Language        | Rust (nightly)          | Zero-cost abstractions, fearless concurrency   |
| Parallelism     | `rayon`                 | Work-stealing, maps to causal frontier draining|
| Concurrent maps | `dashmap`               | Lock-sharded chunk storage                     |
| Async I/O       | `tokio`                 | Battle-tested async networking                 |
| MC Protocol     | `azalea-protocol` 0.15  | Full MC 1.21.11 packet codec                   |
| Causal graph    | `slotmap`               | Dense arena for event DAG nodes                |

---

## Milestones

| Date       | Milestone                                                    |
|------------|--------------------------------------------------------------|
| 2026-02-07 | Phase 0-2: Engine, tests, parallel scheduler, workspace split|
| 2026-02-07 | Phase 3: **First real MC 1.21.11 client connection**         |
