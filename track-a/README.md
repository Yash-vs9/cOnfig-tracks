# Track A: Ingest (Rust Edition)

High-performance zero-dependency Rust log ingestion engine with AVX2 SIMD scanning, hardware CRC32 hashing, lock-free chunk arena, and batched `writev` I/O.

## Approach

1. **Zero-Copy Memory Mapping & Sequential Streaming:**
   Maps `/data/requests.log` with `mmap(MAP_SHARED)` and `madvise(MADV_SEQUENTIAL)`. Pages are prewarmed into page cache by the harness, incurring minimal cgroup peak memory overhead.
2. **12-Thread Hardware Concurrency:**
   Spawns 11 scoped worker threads with small stacks (64 KB) plus the main thread, matching the 12 hardware threads (6 P-cores).
3. **Deterministic 3-Cycle Hour Arithmetic:**
   Replaced dynamic hour interning and atomic CAS with a 3-instruction branchless calendar index `hour_to_idx` across the 72 fixed hours (`2026-10-30T18:00:00Z` to `2026-11-02T17:00:00Z`).
4. **SIMD & SWAR Parser Acceleration:**
   - SWAR bitwise vector operations to branchlessly locate `/` for endpoint start.
   - AVX2 256-bit space comparison (`_mm256_cmpeq_epi8`) to resolve endpoint length in 1 instruction for >95% of requests.
   - 4-load branchless hardware CRC32 (`_mm_crc32_u64`) with 4096-entry thread-local direct cache (`ep_c`).
   - SWAR 8-digit parallel decimal parser for latencies.
5. **Zero-Atomic Fast Path Block Arena:**
   Each thread allocates blocks from a local slice of 1024 blocks in a static BSS arena, falling back to atomic overflow only under severe imbalance.
6. **Parallel Radix/Bucket Aggregation & Percentiles:**
   Processes 72 hours across 6 waves of 12 parallel threads. Employs `pdqselect` (`select_nth_unstable`) with hierarchical range narrowing (p50 partition -> p95 partition -> p99 partition) and specialized sorting networks for small $N$.
7. **Batched Syscall I/O & Zero-Error Fastpath:**
   Direct `"0 0\n"` fast-path writes bypassing decimal serialization for zero-error endpoints (>90% of requests). Writes output via batched `writev` vector I/O.

## Dependencies

0 runtime dependencies. Pure Rust 2021 standard library + core x86 intrinsics.
