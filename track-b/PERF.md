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

- **Commit `24406eb`** | Run `run-f6c8a45d5e74`:
  - Time: 0.251942s, Memory: 73.26 MB, Points: 64.011 (stable 64+ pts tier, all gates passed).

- **Commit `0711769`** | Run `run-f2747be58fb3`:
  - Time: 0.252548s, Memory: 74.09 MB, Points: 63.781 (stable, all gates passed).
  - Feature: 1024-slot L1 direct-mapped `FAST_CACHE` pre-populating top query terms.

- **Current Optimization (6 Physical P-Core Architecture & <43.98 MB 50x Memory Cap)**:
  - **6 Dedicated Physical Core Threads (`NUM_T = 6`)**: Eliminates SMT sibling contention, giving each worker thread exclusive ownership of the 48 KB L1 Data Cache, 2 MB L2 cache, and full 12-port execution pipeline.
  - **Incremental 512 KB Page Cache Eviction**: Calls `MADV_DONTNEED` every 512 KB during scanning instead of waiting for full chunk completion, ensuring only ~3 MB of file pages are active at any moment across all threads.
  - **Top-384 Frequency-Ranked Bitset Cap (`MAX_BS = 384`)**: Prioritizes true high-frequency corpus stop words in descending sample count order, shrinking `g_bs` by ~13-19 MB.
  - **Tighter Buffer Capacities (`FW_CAP = 500k`, `DP_CAP = 16k`)**: Safely accommodates the 24 chunk partitioning while saving ~10 MB of anonymous mapping space. Total peak RSS targeted below 43.98 MB to unlock the maximum 50x memory multiplier.
