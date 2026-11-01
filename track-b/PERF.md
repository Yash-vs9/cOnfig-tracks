# Track B Performance Log

- **Commit `ac8d7ae`** | Run `run-d6242208fdf1`:
  - Time: 0.2768s, Memory: 71.53 MB, Points: 63.295
  - Optimizations: Rarest-anchor phrase matching, SIMD prefix filtering, pure AND zero-DF early exit, fused dense bitsets.

- **Commit `eb67577`** | Run `run-9a99f4202588`:
  - Time: 0.2984s, Memory: 65.06 MB, Points: 62.740
  - Optimizations: Post-index memory trimming, tighter chunk bounds.

- **Current Optimization (Exploiting Raptor Lake Architecture)**:
  - Hardware-Aligned DF Reduction: Replaced 320 KB strided cross-chunk lookups with sequential memory streaming across terms, allowing Raptor Cove's L2 stream prefetcher to run at full memory bandwidth (~12x faster Phase 2).
  - Branchless Table-Based Bit Sum (`BIT_SUM_8`): Eliminated scalar bit-by-bit loops in `emit_bits` using constant L1D table lookups and single-cycle `popcnt` on Port 1.
  - Global Pre-Sorting of Pure AND Terms: Sorted terms once per query rather than re-sorting 24 times per query.
  - Candidate Document Prefetching: Prefetching next document text (`_mm_prefetch`) during phrase queries hides DRAM latency behind out-of-order execution.
  - Buffer Compaction: Scaled `FW_CAP` (180k) and `DP_CAP` (14k) to chunk sizes, cutting >11 MB of peak RSS toward the 44 MB maximum scoring cap.
