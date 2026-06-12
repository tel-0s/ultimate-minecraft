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
- [x] Incremental frontier tracking (see Phase 6a)
- [x] Event deduplication (see Phase 6a)

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

### 4b -- Biomes + composable surface rules
- [x] **`Biome` enum** (Stage 4b starter set: plains, forest, desert, snowy_plains,
      stony_peaks, beach, ocean, river) with stable wire IDs matching the
      configuration-phase biome registry.
- [x] **Climate noise** (`worldgen::climate`): temperature + humidity 2D
      density-function fields, fully JSON-driven via the same `DensityFnSchema`.
      Continentalness / erosion / peaks-and-valleys come in a later stage.
- [x] **`BiomeSource` trait** + `MultiNoiseBiomeSource` (climate + elevation →
      biome via a hand-coded decision table) + `FixedBiomeSource` (for
      superflat / tests).
- [x] **Composable surface rules** (`worldgen::surface`): `SurfaceRule` trait
      + atoms (`block`) + combinators (`sequence`, `condition`). Conditions:
      `at_surface`, `depth_at_most`, `above_y`, `below_y`, `in_biome`,
      `above_water`, `below_water`. Mirrors vanilla's `surface_rule` data files.
- [x] `DensityPipeline` consumes biome + surface rule per column. The previous
      hand-rolled stratification is now a default `surface_rule` in the preset
      JSON.
- [x] Chunk packet sends biomes per **4×4×4 cell** via indirect biome
      paletted containers. Single-valued fast path when all 64 cells share a
      biome; otherwise indirect palette with `ceil(log2(palette_len))` bits
      per entry. Biome edges now fall on 4-block boundaries instead of
      16-block chunk borders.
- [x] Built-in `noise` preset + `presets/amplified.json` updated with
      multi-noise biomes and per-biome surface rules.
- [ ] Per-biome height profiles (climate-driven density adjustment — currently
      all biomes share the same height field). Comes with multi-noise climate.

### 4c -- Multi-noise climate + per-biome height profiles
- [x] **`spline` density-function atom**: piecewise-linear interpolation of
      an input value through a list of `(input, output)` points.
      Endpoints clamp; unsorted input lists are sorted at build time.
      Y-independent when the input is, so the heightmap shortcut still
      applies to spline-driven height fields.
- [x] **Continentalness + erosion noise** in the default `noise` preset:
      base height is now a spline of continentalness (most of the range
      ≈ y=60-85 land, tails to y=35 deep ocean and y=145 peaks); hill
      relief amplitude is a spline of erosion noise (more variation
      where erosion is "jagged", flatter where it's "eroded"). The
      world now has macro-scale continents and mountain ranges driven
      by climate noise, not just hills + base offset.
- [ ] Peaks-and-valleys noise (ridge sharpness modulation).
- [ ] Per-biome density overrides (e.g. plains biome forcibly flattens
      its terrain regardless of erosion noise).
- [ ] Expand biome set toward vanilla coverage.

### 4d -- Caves & ores
- [x] **Carver framework** (`worldgen::carver`): `Carver` trait + JSON
      schema (`carvers: [...]` array at the preset level). Carvers run as
      a post-pass after heightmap stratification, mutating the chunk in
      place. They skip bedrock, water, and air so the world floor stays
      solid, oceans don't drain, and air cells aren't re-processed.
      Architectural choice: keeping caves as a post-pass (rather than
      baking 3D noise into the main density function) preserves the
      heightmap shortcut.
- [x] **NoiseCarver**: 3D-noise density + threshold + y-range. Cheese
      caves out of the box; spaghetti tunnels / ravines come with more
      specialised density-function compositions.
- [x] Default `noise` preset ships a single cheese-cave carver
      (`seed_offset: 500`, frequency 0.035, threshold 0.55,
      y ∈ [-56, 55]).
- [x] **Decorator framework** (`worldgen::decorator`): `Decorator` trait
      + JSON schema (`decorators: [...]` array at the preset level).
      Decorators run after carvers, each with a deterministic per-chunk
      PRNG (SplitMix64) seeded from `(world_seed, cx, cz, decorator_index)`.
- [x] **Ore decorator**: random-walk vein placement with substrate
      filter. Default `noise` preset ships coal / iron / copper / gold /
      redstone / lapis / diamond ores at vanilla-ish frequencies and
      Y-band distributions.
- [ ] Worley/cellular noise atom for vanilla-style spaghetti tunnels.
- [ ] Aquifers (water-filled cave regions).
- [ ] Weighted Y distributions for ores (vanilla peaks ore density at
      specific bands rather than uniform within a range).

### 4e -- Decorators & structures
- [x] **Tree decorator** (oak, MVP): trunk + 3-layer canopy
      (5×5 bottom, 5×5-minus-corners middle, 3×3 top), placed only on top
      of a configured `surface_block`. Trees clip at chunk borders for
      now; deferred cross-chunk writes come with the next decorator
      iteration.
- [x] **Per-biome decorator filter** (`in_biomes: ["plains", "forest"]`)
      on Ore and Tree schemas. Decorator API restructured to
      `Decorator::decorate(&mut DecorationContext)` carrying the chunk,
      biome source, surface-Y grid, sea level — so filters/conditions
      have everything they need without re-running the density pipeline.
- [x] Default `noise` preset ships **biome-varied trees**: oak in
      plains/forest, extra-dense oak + birch in forest, tall spruce on
      snow_block in snowy_plains.
- [x] **Deferred cross-chunk writes** (`worldgen::decorator::PendingWrites`):
      a shared `DashMap<ChunkPos, Vec<PendingWrite>>` on the pipeline
      threaded through `DecorationContext`. New `ctx.set_world_block` /
      `set_world_block_if_air` route writes to:
      (a) the in-flight chunk if local,
      (b) `world.set_block` if the target is already loaded,
      (c) the pending queue if the target chunk doesn't exist yet.
      Each chunk drains its pending list at the end of generation, so a
      tree placed in chunk A whose canopy reaches into B picks up
      regardless of which chunk generates first. Tree canopies now spill
      cleanly across chunk borders instead of being clipped.
- [ ] **Live-broadcast cross-chunk writes**: when a decorator mutates a
      chunk that's already loaded *and* already sent to clients, the
      write doesn't propagate to those clients (no `BlockUpdate` packet
      sent). Affects late-pregeneration neighbours of already-streamed
      chunks. Minor visual quirk; fix by hooking the pipeline into the
      event bus.
- [x] **Plants** (flowers + grass MVP): scattered single-block features
      placed one cell above a configured `surface_block`. `blocks` is a
      weighted list (duplicates bias the draw). Default `noise` preset
      ships grass + dandelion / poppy / oxeye_daisy / cornflower in
      plains, grass + fern + occasional poppy in forest.
- [x] **Tune flower frequency** to match vanilla (mostly grass with rare
      flower scatter). Plant schema now supports weighted entries
      (`{"block": ..., "weight": N}` alongside bare names); default preset
      runs plains at grass:flowers 36:4 and forest at 12:4:1
      (grass:fern:poppy).
- [ ] Kelp, sugarcane, sea grass, etc. — needs adjacency-aware placement
      (sugarcane wants water; kelp wants ocean column).
- [ ] Simple structures (villages, dungeons)
- [x] **Stitched-world bug fixed** (2026-06-09): saved chunks from older
      generator versions loaded verbatim next to freshly-generated chunks,
      producing hard chunk-aligned height/biome seams. Two compounding causes:
      (1) decorator cross-chunk writes used `world.set_block`, marking
      pristine terrain chunks dirty so *generation itself* got persisted
      (360 chunks in one test session) — now `world.set_block_untracked`;
      (2) persistence had no generator versioning — saved chunks now carry a
      `UmcGenFp` NBT stamp (FNV-1a of canonical preset JSON + seed,
      `worldgen::preset::fingerprint`) and chunks with a missing/mismatched
      stamp are skipped at load (terrain regenerates; their block edits are
      discarded — true diff-based persistence is the Phase 6c
      delta-encoding item). Diagnostic `examples/diag_continuity.rs`
      measures border-vs-interior height/biome continuity and proved the
      generator itself is seam-free.

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

- [x] **Causal graph pruning / garbage collection**
      Opt-in via `CausalGraph::with_pruning()` (used by all production cascade
      sites; plain `new()` retains nodes for tests/DOT export). An executed node
      is reaped once all its children have executed: `mark_executed` re-checks
      the node's parents, and the scheduler calls `graph.finish(id)` after
      inserting an event's consequents so leaf events reap immediately.
      Readiness checks treat missing parents as executed (only executed nodes
      are ever reaped). Memory is bounded to the active wavefront — at
      quiescence the graph is empty. Companion: an execution-ordered
      **write log** (`graph.write_log()`) records effective `BlockSet` /
      `LightSet` payloads so `event_bus::collect_*` no longer scans nodes —
      this also fixes change-broadcast ordering, which previously depended on
      SlotMap iteration order. Lifetime counters (`inserted_total`,
      `executed_total`, `reaped_total`) survive pruning for metrics.

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

- [x] **Idempotent rule guards** (stale-precondition skip)
      `apply_event` only applies a `BlockSet` when the current world state still
      matches the event's recorded `old` value. A mismatch means a
      causally-unrelated event already changed the cell — the stale write is
      skipped entirely (no world write, no consequents, no write-log entry).
      Prevents redundant cascades *and* a block-duplication bug where two
      spacelike cascades racing to move different blocks into the same cell
      would both "succeed". First write wins; the loser's rule re-evaluation
      never fires because nothing changed.

### 6b -- Concurrency model (unlock true multi-core scaling)

Replace the current per-connection isolated graphs and DashMap locking with a
shared-nothing spatial ownership model. **Strategic constraint: the partition
boundary is designed as a transport-agnostic message protocol from day one** —
the boundary between two workers on one socket, two sockets on a NUMA
machine, and two machines in a cluster (6f) must be the same abstraction.

#### 6b-0 -- Baseline measurement ✓ (2026-06-09)

`cargo run --release --example bench_baseline` — scenarios × {seq, par},
verifying world-state equality. Instrumentation added to `CausalGraph`:
`edge_locality()` (same-chunk vs cross-chunk causal edges — cross-chunk
edges become inter-partition messages under 6b-2) and `peak_len()`
(wavefront high-water mark under pruning).

Baseline on a 32-logical-core machine (Windows, release):

| scenario     | events  | seq ms | par ms | speedup | ev/s (seq) | locality | peak wave |
|--------------|---------|--------|--------|---------|------------|----------|-----------|
| sand-rain    | 242,172 | 84.6   | 76.5   | 1.11×   | 2.86M      | 100.0%   | 61,507    |
| water-flood  | 59,200  | 21.0   | 20.1   | 1.05×   | 2.81M      | 93.5%    | 25,600    |
| border-flood | 74,000  | 26.3   | 22.9   | 1.15×   | 2.82M      | 85.0%    | 32,000    |
| torch-grid   | 66,060  | 68.3   | 25.3   | 2.69×   | 0.97M      | 84.1%    | 66,060    |
| mixed        | 97,429  | 84.6   | 38.9   | 2.18×   | 1.15M      | 66.8%    | 76,221    |

Aggregate: 538K events, speedup **1.55× on 32 cores (~5% parallel
efficiency)**, edge locality **90.1%**. Single-action quiescence latency:
sand 9.8µs, water source 49µs, torch 1.7ms. Propagation velocity: sand
0.51M blocks/s, water 0.14M, torch light 7.6K (roadmap target:
10-30M/s/core — a 20-2000× gap, almost all per-event scheduling overhead).

Findings that shape 6b-1/6b-2:
1. **Cheap-rule workloads don't parallelize at all** (1.05-1.15×): the
   gather phase (mark_executed + insert on `&mut graph`) is serial and
   dominates when rule evaluation is light. Amdahl confirmed — the graph
   mutation path is THE bottleneck, not rule evaluation.
2. **Locality is rule-dependent**: gravity 100%, fluids 85-94%, but light
   is the locality hog — a border-adjacent torch's radius-14 BFS field
   spans up to 4 chunks (realistic mixed load: 66.8%). 6b-2 needs a
   partition-aware light strategy (halo reads or light-field handoff), not
   just block-event routing.
3. **Peak wavefront = rule fanout × drain batch size** (torch-grid
   materializes 100% of its events in one step). The 6b-1 batch-submission
   window doubles as the physics memory-bound knob.
4. **Interacting water fronts are not confluent** (discovered by the
   harness): two fronts meeting settle at different, both-locally-stable
   levels depending on arrival order — the fluid rule never lowers
   existing water. Sequential frontier reorderings already exhibit this;
   it's a rule-semantics gap (vanilla re-levels to min-neighbor+1), not a
   scheduler bug. Must be fixed for causal invariance to hold under 6b's
   continuous shared-graph execution. Until then the harness uses
   non-interacting feature grids.
5. Event counts are schedule-dependent (±0.1%) via notify-dedup batching;
   benign — world state is the invariant, and it verifies.

#### 6b-1 -- Decoupled physics service ✓ (2026-06-09)

`physics.rs`: one dedicated OS thread owns THE shared `CausalGraph`
(pruning) + `RuleSet` and applies all world writes. Connections and
simulation layers are pure event sources via `PhysicsHandle::submit_*`
(unbounded mpsc; never blocks the sender). All changes broadcast as
`ChangeSource::Physics` bus batches consumed by every client — the
originator included, so there is exactly one mutation→packet path
(connections just ack the action sequence immediately). Stair-shape
rewrites run as a post-cascade hook inside the service. The engine
gained `take_write_log()` so the long-lived graph drains its log per
batch.

What this resolved:
- **Cross-player causality**: all actions feed one DAG (covered by
  `tests/physics_service.rs`: A's sand falls into the hole B breaks).
- **Non-blocking player I/O**: cascades no longer run on connection tasks.
- **Batch submission** (was a separate 6b item): commands arriving while
  a batch processes are drained together into the next one — an emergent
  window set by processing time, not a tick.

Known limitations, deliberate until 6b-2/6d:
- One physics thread is a serialization point; `step_parallel` still
  fans out rule evaluation inside it, but graph mutation is single-owner.
  6b-2 splits it into N region owners behind the same handle interface.
- A monster cascade delays everyone's physics (no priority/interleaving
  yet — that's 6d's priority-aware draining).
- Actions are acked before the cascade commits; a stale-dropped action
  desyncs the acting client until the next chunk reload (rare race,
  same exposure as the previous inline design).

- [x] **Shared causal graph**
      Done in 6b-1 via the dedicated graph-owner thread + channel interface
      (the roadmap's option B). Concurrent insertion (option A) deferred —
      6b-2's per-partition graphs make it unnecessary.

- [x] **Decoupled physics from connection handler** (6b-1, above)

- [x] **Batch event submission** (6b-1, above — emergent drain window)

#### 6b-2 -- Partitioned causal scheduling ✓ (2026-06-09)

N physics workers each own a disjoint set of **4×4-chunk regions**
(deterministic SplitMix-hash assignment, `physics.workers` in
server.yaml, 0 = auto). Each worker has a private pruned `CausalGraph` —
graph mutation, the bottleneck 6b-0 identified, is now unshared. The
partition boundary is a **message**: `Scheduler::step_routed` hands each
consequent to a router; foreign-chunk events are forwarded over the
owner's channel *after their cause executed*, so happens-before rides the
transport (the same protocol later runs over sockets for 6f). Global
quiescence via an in-flight message counter (`PhysicsHandle::pending()`;
forwards are counted before their consuming batch decrements, so 0 ⇒
done).

Scaling vs the same workloads at 1 worker (32-core machine,
`cargo run --release --example bench_partitioned`):

| scenario    | 1w ms | 16w ms | speedup | ev/s @16w | 6b-0 (old sched.) |
|-------------|-------|--------|---------|-----------|--------------------|
| sand-rain   | 96.4  | 15.0   | 6.4×    | 16.1M     | 1.11× at 32 cores  |
| water-flood | 43.5  | 7.9    | 5.5×    | 10.6M     | 1.05×              |
| mixed       | 92.7  | 16.4   | 5.7×    | 6.3M      | 2.18×              |

Block state verified **identical across all worker counts** — partitioned
execution is deterministic up to light (see exceptions below).

Correctness work this required:
- **Fluid confluence** (re-level rule): a flowing cell relaxes to
  `min(neighbor)+1` on notify (drains when unfed or past the cap), making
  the fluid fixed point unique — interacting fronts now settle
  identically under any execution order (`interacting_water_fronts_are_confluent`
  covers reversed/shuffled/parallel schedules).
- **Self-stabilization under stale cross-partition reads**: fluid
  *appearance* also notifies adjacent same-kind fluid. Found by the
  scaling bench at 16 workers: a boundary cell could drain against a
  pre-write read of a foreign neighbour and — since spread only targets
  air — never be revisited. The appearance-notify is emitted after the
  write commits, so the wrongly-drained cell re-evaluates with the write
  visible. Cost: ~40% more (cheap, dedup-coalesced) events in
  fluid-heavy cascades; single-worker water is ~2× slower than the old
  non-confluent rule. Correctness buys it.

Known limitations / next:
- Speedup flattens 8→16 workers (36 regions over 24×24 chunks → load
  imbalance; plus shared-DashMap bandwidth). 6d's adaptive region sizing
  and work stealing, and 6c's per-partition arenas, attack this.
- Light BFS still writes across partitions directly (documented
  ownership exception; races converge since light recomputes from block
  state). Needs a partition-aware strategy.
- Stair-shape rewrites may write a foreign chunk at radius 1 (rare,
  idempotent, direct).

- [x] **Chunk ownership partitioning** (6b-2, above). The original sketch
      called for replacing the `DashMap` itself; 6b-2 partitions *event
      execution and graph ownership* while cross-boundary reads still go
      through the shared `DashMap` — moving chunk storage into per-worker
      arenas is Phase 6c (locality-aware allocation).


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

- [x] **Compact block state representation** ✓ (2026-06-09)
      `ChunkSection` is now paletted in RAM: unique blocks once + packed
      indices at 0 (uniform) / 4 / 8 / 16 bits per cell, widening on
      demand, with an O(1) `non_air` counter (`is_empty` was an O(4096)
      scan on every air-write). Measured on the noise preset
      (`cargo run --release --example bench_memory`): **8192 → 2061
      B/section average, 4.0× reduction** (40.6 → 10.2 KB of block data
      per chunk); every natural section fits 4-bit indices (max palette
      14). Throughput unchanged (`bench_partitioned` within noise).
      Chunk-send fast path: single-entry palette → single-valued wire
      section with no scan. Palettes only grow (stale entries linger
      until a future compaction pass); serialization rebuilds exact
      palettes.

- [x] **Delta-encoded persistence** ✓ (2026-06-09) — the durable half of
      delta storage. Dirty chunks save as cell-diffs vs the regenerated
      procedural baseline (`UmcDelta` packed-i64 NBT array; one edited
      block = one entry instead of a full chunk). Loading regenerates and
      re-applies — so **block edits now survive preset/seed changes**
      (the fingerprint-skip rule applies only to legacy full-section
      chunks). Save cost: one baseline regeneration per dirty chunk
      (~ms, on the autosave task).
- [x] **Delta-encoded runtime storage / chunk eviction** ✓ (2026-06-09):
      memory is now bounded by ACTIVE area, not explored area. The server
      worldgen is a `DeltaOverlayGen` (baseline + live `DeltaStore`,
      populated by load and refreshed by every save), so any chunk whose
      edits are saved is exactly reproducible. A periodic sweep
      (`eviction.rs`, `world.eviction_interval_secs`, keep radius
      `world.keep_radius`, 0 = view_distance + 8) drops non-dirty chunks
      beyond the keep radius of every player + spawn; the next
      `ensure_generated` brings them back bit-for-bit
      (`test_eviction_roundtrip_through_overlay`). Dirty chunks wait for
      autosave. Known coarseness: an in-flight cascade touching a chunk
      at eviction reads AIR and self-heals via the stale guard.

- [x] **Hot-path access costs measured & fixed** ✓ (2026-06-09,
      `bench_access`): the per-read DashMap lookup is 15.5 ns = 59% of a
      block read; a last-chunk memo recovers 66% even at 76% hit rate —
      so the strategy is lookup *amortization*, and full per-worker chunk
      arenas stay deferred until contention (not per-access cost) is
      measured. Applied: chunk-memoized light BFS (`CachedWorld`),
      LUT-backed block light properties (azalea `Box<dyn BlockTrait>` +
      string match per call → one-time 27 KB tables), and **`LightBatch`**
      (one reporting event per light flood instead of ~1,800 per-cell
      `LightSet`s — graph bookkeeping was ~95% of a torch placement).
      Net: torch place 1.7 ms → **371 µs**, light propagation 7.6K → 35K
      blocks/s, torch-grid scenario 66,060 → 72 events.

### 6d -- Scheduling & work distribution ✓ core (2026-06-09)

- [x] **Priority-aware frontier draining**
      `CausalGraph` has a two-lane ready queue: player actions enter at
      priority 1, background physics at 0, children inherit the max of
      their parents (the whole cascade stays in the fast lane), and
      forwarded cross-partition events keep their lane. Priority only
      reorders spacelike-separated events — causal order is enforced by
      parent edges — so outcomes are unchanged, only latency. Combined
      with **per-step publishing** (workers broadcast changes after every
      scheduler step instead of at batch quiescence), a player's block
      break reaches clients while a large background flood is still
      cascading — proven by
      `priority_action_publishes_before_background_flood_finishes`.

- [x] **Adaptive region rebalancing + hot-region splitting**
      Region→worker assignment is now a *table* (default = deterministic
      hash; overrides installed by a rebalancer thread, snapshot-read by
      workers each loop iteration). The rebalancer meters per-region write
      throughput every 50 ms and, with hysteresis (one change per tick +
      500 ms per-region cooldown): **moves** a hot region from the busiest
      worker to the idlest, **splits** a region carrying >50% of total
      load into per-chunk ownership across all workers, and reverts cooled
      splits. During a handoff both owners may briefly execute events for
      the same region — the same race class as a partition boundary,
      tolerated by the stale guard + confluent rules.
      Measured (32-core, `bench_partitioned` hotspot: sustained load
      confined to ONE region, 8 workers): static 161 ms → adaptive 101 ms
      (**1.60× whole-run; the split engages after the first metering
      tick, roughly doubling post-split throughput 2.8 → 4.6M ev/s**).
      Config: `physics.rebalance` (default on).

- [x] **Work-stealing across partitions** — subsumed: under exclusive
      chunk ownership, "stealing" an event would violate write ownership;
      the ownership-compatible equivalent is *moving the region itself*,
      which the rebalancer does. Fine-grained intra-batch stealing may
      return later inside a worker pool per NUMA node.

- [x] **Worker→core pinning** (`physics.pin_workers`, default off):
      first step toward NUMA-local memory; per-partition arenas pinned to
      the owning core's memory node remain (6c residue, with eviction).

### 6e -- Measurement & validation

- [x] Load testing: **1k simulated players** ✓ (2026-06-09;
      `examples/load_test.rs` — a headless protocol-client swarm:
      handshake/login/configuration/play, keep-alive replies, optional
      wander + block-breaking). Results (32-core desktop, release):
      - **1,000 idle joins**: 1000/1000, 0 errors; join p50 1.1 s / p99
        4.5 s (a "join" = full delivery of 289 chunks ≈ 12 MB each);
        289,000 chunks streamed = **11.9 GB at 195 MB/s sustained**;
        ~5.5 cores; **57 MB RSS total** (≈45 KB/player marginal).
      - **100 wander+dig**: 46K block updates fanned out, 0 errors,
        p50 join 149 ms, ~2.5 cores.
      - **500 wandering**: 0 errors, 6,085 chunks/s (268 MB/s), ~6 cores
        — but ~380K player-event drops (see finding 3).
      Findings (each fixed or filed):
      1. **Async/lock wedge (fixed)**: DashMap read guards held across
         packet-write awaits in `send_chunk_from_world`/`send_light_updates`
         + `ensure_sky_light`'s write lock = all tokio workers blocked on
         locks whose holders couldn't be scheduled. 1,000 joins → total
         stall, ~0 CPU. Guards now drop before every await.
      2. **Sky-light thundering herd (fixed)**: hundreds of joiners all
         passed the lock-free `is_sky_lit` check and queued to rescan the
         same spawn chunks; now re-checked under the chunk lock and marked
         before guard release.
      3. **O(N²) movement fan-out (filed)**: every move broadcasts to
         every connection (≈1.25M deliveries/s at 500 wanderers); the
         per-connection player-event channel drops under load (visual
         entity-position staleness, self-healing). Needs **interest
         management / AOI filtering** — broadcast only to players in
         range. Natural companion of 6f's gateway-node split.
      Bus capacity raised 256 → 8192 (Arc-backed slots; megabytes of
      worst-case buffer). 10k/100k players need AOI first.
- [x] Load testing: **10k simulated players** ✓ (2026-06-12; 4 swarm
      processes × 2,500 against one server). Final:
      **10,000/10,000 joined, 2,890,000/2,890,000 chunks delivered
      (100%), 126.2 GB streamed at 529 MB/s peak, p50 join 1.3 s /
      p99 10 s, 2.7 GB peak RSS; 10k idle players ≈ 4% of one core.**
      Three walls found and fixed (each fix exposed the next):
      1. **Synchronous join streaming**: the initial 289-chunk send ran
         before the main loop, so queued clients could sit >30 s with
         zero packets and time out (vanilla clients kick at 30 s). Fixed:
         initial load goes through the deferred chunk queue
         (`immediate_radius` default 2) with keep-alives interleaved;
         tab-list snapshot collapsed to ONE multi-entry packet;
         join/leave lifecycle bursts coalesced per drain.
      2. **Fairness without admission**: with everyone streaming
         concurrently, per-client throughput fell below one 43 KB chunk
         packet per 30 s — the client timeout fires on PACKET completion,
         so *everyone* starved. Fixed: `network.stream_permits` (256), a
         global admission semaphore — N connections bulk-stream in fast
         waves while the rest idle safely on keep-alives.
      3. **Presence is O(N²) bytes**: 10k joiners × ~1.2 MB of tab
         entries + entity spawns to each of 10k recipients ≈ 12 GB of
         join-storm presence traffic choked the write plane (13 MB/s,
         30 GB RSS in parked writes, mass starvation). Fixed:
         `network.tab_list_cap` (500) + `network.entity_spawn_cap` (200)
         with per-connection tracking so removals stay consistent —
         the static sibling of the move-broadcast O(N²) that spatial
         pub/sub solved in 6f. Proper AOI entity lifecycle supersedes
         the caps with Phase 5 entities.
      Residual: ~2% of clients hit tail keep-alive stalls during peak
      streaming (10k ready tasks × ms-scale batches → tail timer
      latency); gateway sharding (6f) is the designed answer.
- [ ] Load testing: 100k simulated players (needs multi-node gateway
      deployment — single-box residual above)
- [x] Microbenchmarks: events/sec/core, quiescence latency by cascade type
      -- `examples/bench_baseline.rs` (see 6b-0 for current numbers)
- [x] Causal propagation velocity metric (blocks/sec) as a first-class benchmark
      -- per-action front-distance / quiescence-time in `bench_baseline`
- [ ] Formal causal invariance proof for full rule set
- [x] Comparison metrics vs. traditional tick-based architecture
      ✓ (2026-06-09, vs REAL vanilla 1.21.11 server.jar measured on the
      same machine — console-driven via `bench_vanilla/*.ps1`, mirrored by
      `examples/bench_vs_vanilla.rs`):
      | workload | vanilla (measured) | ours (16 workers) | speedup |
      |---|---|---|---|
      | 441 water ponds settle | 1.75 s (rule floor, 20.10 TPS) | 13.6 ms | **128×** |
      | 10,000 sand, 29-block fall | 2.20 s (44 gt kinematics, 20.05 TPS) | 194 ms | **11×** |
      | 160,000 sand, 16 layers | ≥2.85 s floor, done <12.3 s (20.07 TPS) | 387 ms | **≥7×** |
      | single block-break cascade | 50 ms-1.75 s (next tick → rule time) | 9.8-49 µs | **~10⁴-10⁵×** |
      | water propagation velocity | 4 blocks/s (hard rule cap) | 137K blocks/s | **~34,000×** |
      Key finding: vanilla held 20 TPS on EVERYTHING up to 160K falling
      entities — its tick architecture *rations* world change (rules
      meter cascades across real-time ticks) rather than racing it. So
      vanilla is rule-bound, not CPU-bound: faster hardware cannot
      improve its numbers, while more cores directly improve ours. Bulk
      speedups (7-11×) are CPU-bound on our side and rise with the
      remaining 6c/6f work; latency/propagation speedups (10⁴-10⁵×) are
      architectural and already banked.
- [ ] Memory profiling: per-player footprint, graph growth rate, arena utilization
- [ ] **Resource-usage comparison vs vanilla** (companion to the speed
      comparison above): RSS under identical workloads and world sizes,
      per-loaded-chunk and per-player memory footprint, idle CPU cost
      (vanilla burns a core ticking an empty world; we should be ~0),
      startup time. Vanilla side measurable with the same
      `bench_vanilla/` console-driver approach + process counters.
- [ ] Flame graphs for event processing hot path
- [ ] Cross-player causality integration tests

### 6f -- Distribution

#### 6f-0 -- Two-process prototype ✓ (2026-06-09)

The partition boundary now runs over a socket. `cluster.rs`: manual
binary codec for events (incl. `LightBatch`), framed TCP link (blocking
reader/writer threads), and four frame kinds — `Forward` (cross-node
consequents, sent post-execution so happens-before rides the socket
exactly as it rode the channel), `Action`, `WriteSync` (each node
mirrors its executed write log; the peer applies it to a replica world
and republishes on its bus so ITS clients see remote physics), and
`Ping`/`Pong` (two-node global quiescence via matched sent/received
counters with receive-before-count ordering).

What makes it cheap: **deterministic worldgen** means every node
generates identical baseline terrain locally — chunks are never
transferred; and **confluent, self-stabilizing rules** (6b-2) make
replica reads at node borders the same tolerated race class as
cross-partition reads in one process. One ordering rule matters:
WriteSync flushes BEFORE Forward within a step, so a forwarded event's
causal prerequisites are in the peer's replica when it arrives.

Measured (`bench_cluster`, spawns a real `physics_peer` process,
sand-rain across a 24×24-chunk arena, regions split ~50/50):
- **World bit-identical to the single-process run** (verified per cell;
  also `tests/cluster.rs`: cross-node action mirrors back; mixed
  sand+border-water workload matches single-node on BOTH nodes, 700K+
  cells, water cascading across the socket mid-flow).
- Node 0 executed exactly 50% of events; the peer the rest.
- Protocol overhead on one machine: **within noise** (34.2 ms vs 36.8 ms
  single-process — the boundary costs nothing for region-local work).

#### 6f-1 -- N-node mesh + migration + interest management ✓ (2026-06-09)

- **N-node mesh** (`ClusterMesh`): one link per peer, formed
  symmetrically (dial lower ids, accept higher; dialers identify with a
  `Hello` frame). WriteSync broadcasts to all peers. **N-node global
  quiescence**: `Pong` reports each node's pending plus its
  sent/received totals across ALL its links, so a coordinator detects
  in-flight traffic on links it can't see; quiet ⇔ all pendings 0 ∧
  Σsent == Σreceived, two stable rounds. Verified: a **3-node mesh over
  real TCP** converges to the single-node world exactly (water crossing
  node borders included).
- **Region migration** — the replica design's payoff: since every node
  already mirrors every write and regenerates identical terrain, moving
  a region is a **pure ownership flip** (`Transfer` frame, counted, into
  every node's override table). Zero state transfer. The propagation
  window is transient dual-ownership — the same race class as the 6d
  intra-node rebalancer, absorbed by the stale guard + confluence.
  Verified: migrate mid-workload; post-migration waves execute on the
  new owner; all worlds converge. (Single-initiator prototype:
  concurrent conflicting migrations of one region are not arbitrated.
  Cross-node auto-rebalancing can now reuse the 6d metering pattern.)
- **Interest management (entity layer)**: receiver-side AOI filter
  (skip move packets for players outside view distance + margin) and —
  the piece that actually mattered — **entity-tracker coalescing**:
  connections drain move-event bursts and keep only the newest absolute
  position per entity, bounding per-iteration work at one teleport per
  visible player regardless of incoming move rate. 500 wandering
  players: **~380K dropped events → 0**, lossless delivery (the dense
  crowd now costs real CPU instead of dropped updates — the honest
  trade). Sender-side spatial pub/sub (only deliver to subscribed
  regions) remains the gateway-tier upgrade for 10k+.

- [x] Distributed execution across machines (N-node mesh, above)
- [x] Cross-node causal ordering protocol — no vector clocks needed:
      consequents ship after their cause executes, FIFO transport
      carries happens-before, confluent rules absorb replica staleness.
- [x] Region migration for load balancing (ownership-flip protocol,
      above; auto-policy pending)
- [x] **Edge-node architecture + spatial pub/sub** ✓ (2026-06-09 —
      Phase 6f COMPLETE):
      - **SpatialBus** (`event_bus.rs`): region-bucketed pub/sub replaces
        the broadcast firehose for world changes AND entity movement.
        Connections subscribe to the regions inside their view (+2 chunk
        margin), rediffed on chunk-border crossings; physics
        `publish_writes`, cluster `WriteSync` republish, and registry
        moves all deliver per-region — **O(nearby players) per event,
        not O(all players)**. Join/leave/chat stay on the global
        broadcast (tab list is global, low rate).
        Measured (500 wanderers, 40 s): dense crowd 28.5 cores → spread
        crowd **11.5 cores while doing 1.5× the chunk streaming**
        (374K chunks, 411 MB/s) — the entity-delivery plane shrank ~10×;
        the dense case's remaining cost is irreducible (mutual
        visibility).
      - **Gateway nodes**: a gateway is a cluster node with
        `node_id >= physics_nodes` — it owns zero regions, so its local
        physics service receives no work; it serves players from its
        replica world (deterministic worldgen + WriteSync) and submits
        every action over the mesh. Config: `[cluster]`
        enabled/node_id/total_nodes/physics_nodes/listen/peers (see
        `gateway-demo.yaml`). Demo verified end-to-end: 100 protocol
        clients wandering + digging through a gateway, 45,362 block
        updates round-tripped client → gateway → Action frame → peer
        cascade → WriteSync → replica → spatial bus → clients; the peer
        process reported ALL 10,313 physics events executed there.

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
