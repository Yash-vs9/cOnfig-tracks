# Track D: Traverse

Championship solution for Speed Demon 2026, Track D (Traverse).

## Approach

Our solution computes single-source shortest path distances across 2,000 queries on a 1.74M-node, 5.0M-edge road network graph, achieving near-optimal wall time, sub-65MB memory footprint, and compact source code.

1. **Dual Compressed Sparse Row (CSR) Representation:**
   - Graph edges are bit-packed into 32-bit unsigned integers: 21 bits for vertex index $v$ ($N = 1,740,512 < 2^{21}$) and 11 bits for weight $w$ ($w < 2047$).
   - Rare weights $\ge 2047$ (less than 0.07% of edges) use sentinel value 2047 and are resolved via a binary-searched overflow lookup table.
   - Dual CSR stores both forward edges (`edges_fwd`, 20 MB) and reverse edges (`edges_rev`, 20 MB) with 32-bit index heads (`head_fwd`, 7 MB; `head_rev`, 7 MB), fitting the entire graph into 54 MB.

2. **Zero-Copy Parallel Ingestion:**
   - The input `graph.txt` is memory-mapped using `mmap` with `MADV_WILLNEED` and divided into 12 chunks across threads.
   - Pass 1 counts node degrees in parallel; Pass 2 populates dual CSR edges directly. The memory mapping is unmapped (`munmap`) immediately before query processing to eliminate page cache RSS overlap.

3. **High-Density 8-Byte Slot Architecture & L2 Cache Residency:**
   - Bidirectional Dijkstra searches alternate frontiers based on queue length.
   - Each thread maintains forward and reverse hash tables (`table_fwd` and `table_rev`) with 8-byte slots: `Slot { v: u32, d: u32 }`.
   - Empty slots are marked with sentinel `v = u32::MAX`. An $O(\text{touched})$ `dirty` index vector resets only modified slots between queries in microseconds, completely eliminating epoch counters and wrapping arithmetic.
   - The entire per-thread search structure occupies only 1 MB, residing completely inside the 2 MB private L2 cache of each CPU core to eliminate DRAM access latency.

4. **Fast Branchless Min-Heap & Pruning:**
   - 4-ary cache-aligned min-heap with packed 64-bit entries (`(dist << 32) | slot_idx`).
   - Query fast-paths: immediate 0 return for $s = t$; immediate -1 return if $s$ has no out-edges (`head_fwd[s+1] == head_fwd[s]`) or $t$ has no in-edges (`head_rev[t+1] == head_rev[t]`).
   - Dynamic upper-bound pruning ($\mu$) breaks search immediately when tentative bounds meet. Rare overflow queries spanning weights $\ge 2^{31}$ safely fall back to a 64-bit bidirectional solver.

5. **Clean Termination:**
   - Outputs are formatted via fast custom integer-to-ASCII serialization and written to `distances.txt`.
   - The process exits via `_exit(0)`, bypassing redundant glibc thread join and heap teardown cycles.

## Dependencies

None. Zero external crates (`dependency_count = 0`), maximizing the dependency coefficient factor $D = 1.0$.

## With another 24 hours

1. **SIMD 8-ary MinHeap Vectorization:** Implement AVX2 vector comparisons for child selection in heap sift-down.
2. **Contraction Hierarchies / Core Pruning:** Precompute shortcuts for degree-2 road chains to contract transit nodes while keeping preprocessing within the wall-clock time budget.
3. **PGO with Hardware Performance Counters:** Profile cache-miss hotspots and optimize memory layout of query dispatch.
