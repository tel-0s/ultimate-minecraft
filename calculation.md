Great puzzle. Let me work through this from Lloyd's specs down to Minecraft.

## Seth Lloyd's Ultimate Laptop (1 kg, 1 liter)

From his 2000 paper, applying the Margolus-Levitin theorem and Bekenstein bound:

| Resource | Value |
|---|---|
| Max computation | ~10^51 ops/sec (via E = mc^2, then 2E/╧ÇΓäÅ) |
| Max memory | ~10^31 bits (thermodynamic entropy of a 1 kg photon gas at ~10^9 K) |

## Per-Player Minecraft Cost (Ideally Optimized)

"Ideally optimized" means perfect parallelism, optimal algorithms, delta-encoded procedural world -- but still recognizably Minecraft (tick-based, block world, entities, mob AI).

| Resource | Estimate | Reasoning |
|---|---|---|
| **Memory** | ~10^7 bits (~1 MB) | Player state (~10 KB) + share of active chunks (~300 chunks, delta-encoded from worldgen at ~10^3-10^5 bits each) + nearby entities/mobs (~50 mobs ├ù ~1 KB) + buffers. Current servers use ~50-100 MB/player; 100x improvement from removing Java overhead and optimizing data structures. |
| **Computation** | ~10^8 fundamental gate ops/tick | Physics, mob AI (~50 mobs ├ù pathfinding), random ticks for ~300 chunks, entity updates, packet serialization. Roughly 10^6 CPU instructions ├ù ~100 gates/instruction. |

## Part 1: Maximum Concurrent Players

**Memory is the binding constraint.**

$$N_{\text{players}} \approx \frac{10^{31} \text{ bits}}{10^7 \text{ bits/player}} \approx 10^{24} \text{ players}$$

Cross-checking computation at vanilla 20 TPS:

$$N_{\text{compute}} = \frac{10^{51}}{10^8 \times 20} = 5 \times 10^{41} \text{ players}$$

Memory binds long before computation does.

> **Answer: ~10^24 players** -- remarkably close to Avogadro's number (6 x 10^23). One player per molecule in a mole of water, roughly.

Side note: the entire Minecraft world (┬▒30M blocks in X/Z, 384 tall) is only ~10^18 addressable blocks, so at 10^24 players you'd have ~10^6 players per surface block. The world would need to be extended, or this is one *very* crowded server.

## Part 2: TPS with 10^24 Players

Total computation per tick:

$$10^{24} \times 10^8 = 10^{32} \text{ ops/tick}$$

**Naive (no coordination limit):**

$$\text{TPS} = \frac{10^{51}}{10^{32}} \approx 10^{19}$$

**But there's a physical catch.** A Minecraft tick requires global state consistency (entity interactions, block updates propagating, etc.). Light crosses the 10 cm laptop in ~3 ├ù 10^-10 s, capping global synchronization at:

$$\text{TPS}_{\text{light}} \approx \frac{c}{2R} \approx \frac{3 \times 10^8}{0.12} \approx 2.5 \times 10^9$$

With aggressive region pipelining (different chunks at slightly different tick numbers, syncing at boundaries -- essentially treating the game world relativistically), you could push closer to the naive limit. But strict global tick coherence caps you around:

> **Answer: ~10^9 - 10^10 TPS** (conservative, globally-coherent ticks)
> Or ~10^19 TPS if you allow pipelined/relativistic tick regions.

## Putting It In Perspective

Even at the conservative 10^10 TPS, the server runs 5 ├ù 10^8 times faster than vanilla (20 TPS). Every real-world second, each of those ~10^24 players experiences **~16 years** of Minecraft game time.

The fun irony: the ultimate laptop has so much compute that **memory is the bottleneck by 17 orders of magnitude**, and even then, the speed of light -- not computation -- is what ultimately limits tick rate.