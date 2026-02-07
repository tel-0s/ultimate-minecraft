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
- [ ] Incremental frontier tracking (avoid O(N) full-graph scan)
- [ ] Event deduplication (eliminate exponential blowup from notify+set overlap)

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

### Next
- [ ] Block break/place -> injected as causal events -> broadcast to clients
- [ ] Multiple simultaneous players (each sees the other's changes)
- [ ] Player -> client projection: coalesce causal events into block change packets
- [ ] Chunk loading based on player position (send new chunks as player moves)
- [ ] Creative mode inventory
- [ ] Chat messages

## Phase 4 -- World Generation

> Infinite procedural terrain, lazily generated.

- [ ] Noise-based terrain generator (simplex/perlin height map)
- [ ] Biome system (plains, mountains, ocean)
- [ ] Chunk generation on demand (triggered when player approaches unloaded region)
- [ ] Deterministic from seed
- [ ] Cave carving, tree/structure placement

## Phase 5 -- Entities & Physics

> Moving objects integrated into the causal graph.

- [ ] Entity as a causal actor: position updates are events with spatial dependencies
- [ ] AABB collision detection against block grid
- [ ] Gravity, jumping, basic kinematics
- [ ] Entity-entity interaction as causally-ordered events
- [ ] Mob spawning rules, basic mob AI

## Phase 6 -- Scale & Optimization

> Push toward the theoretical limits.

- [ ] Incremental frontier tracking
- [ ] Event deduplication
- [ ] Delta-encoded chunk storage
- [ ] Adaptive region sizing
- [ ] Distributed execution across machines
- [ ] Load testing: 1k, 10k, 100k simulated players
- [ ] Formal causal invariance proof for full rule set
- [ ] Comparison metrics vs. traditional tick-based architecture

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
