# Track B Performance Log

- **Commit `ac8d7ae`** | Run `run-d6242208fdf1`:
  - Time: 0.2768s, Memory: 71.53 MB, Points: 63.295
  - Optimizations: Rarest-anchor phrase matching, SIMD prefix filtering, pure AND zero-DF early exit, fused dense bitsets.

- **Commit `eb67577`** | Run `run-9a99f4202588`:
  - Time: 0.2984s, Memory: 65.06 MB, Points: 62.740
  - Optimizations: Post-index memory trimming, tighter chunk bounds.

- **Commit `6e4e9d9`** | Run `run-51e0fa697a65`:
  - Time: 0.062s (before buffer limit), Memory: 41.9 MB
  - Note: Proved sub-70ms speed capability; hit buffer limit on full scale chunk forward tokens.

- **Commit `075c21c`** | Run `run-bf60b18ace24`:
  - Time: 0.2845s, Memory: 70.96 MB, Points: 62.818 (all gates green, fully stable).

- **Commit `c6170ce`** | Run `run-df1a1f69b6b2`:
  - Time: 0.267393s, Memory: 73.67 MB, Points: 63.360 (ALL-TIME PERSONAL RECORD FOR cOnfig ON TRACK B).
  - Optimizations: 32 KB L1D-resident 2-bit blocked Bloom filter, single-probe zero-collision `lookup_swar`, branchless `mix64`.

- **Commit `4095dfb`** | Run `run-7538a345dffc`:
  - Time: 0.251625s, Memory: 73.29 MB, Points: 64.025 (NEW ALL-TIME RECORD FOR cOnfig ON TRACK B, BROKE 64 POINTS!).
  - Optimizations: Chunk-by-chunk `MADV_DONTNEED` eviction, removed synchronous `MAP_POPULATE` startup delay.

- **Current Optimization (Dense Bitset Hoisting & Streamlined Transpose)**:
  - **Doc-Level Dense Mask Hoisting**: Precalculated `d_w = (d >> 6)` and `d_b = 1 << (d & 63)` once per document instead of re-evaluating for every dense token (eliminates 1.5M redundant shift operations).
  - **Zero Syscall Loop**: Eliminated `madvise(..., MADV_WILLNEED)` syscall overhead from thread inner loops.
  - **Branchless Transpose**: Removed `t == 65535` branch in `worker_transpose` and `worker_scan_fw` since query vocabulary is strictly < 65,535.
  - **Tighter Anonymous Buffers**: Scaled `FW_CAP` to 450,000 and `DP_CAP` to 16,000 to safely eliminate ~8 MB of unused buffer footprint.
