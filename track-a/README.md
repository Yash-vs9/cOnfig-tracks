# Track A: Ingest

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/a-ingest.md).

## Approach

Our solution is a high-performance, single-translation-unit C++23 implementation:
1. **Zero-Copy Memory Mapping:** Maps `/data/requests.log` with `mmap(MAP_SHARED)`, ensuring reading charges 0 bytes to the cgroup v2 memory peak limit.
2. **12-Thread Byte-Range Partitioning:** Divides the mapped buffer into 12 newline-aligned chunks processed concurrently across 12 OpenMP worker threads matching the 6 Golden Cove P-cores.
3. **Two-Stage Lock-Free Sharded Aggregation:**
   - **Stage 1 (Parse & Partition):** Each thread parses lines using SIMD-accelerated newline scanning and backward parsing for latency and status. Computes a 64-bit hash `h` of the 13-byte hour and endpoint using hardware CRC32 intrinsics (`_mm_crc32_u64`), routing records lock-free into 12 thread-local shard buffers (`shard_buf[tid][h % 12]`).
   - **Stage 2 (Local Aggregation & Nearest-Rank Percentiles):** After a barrier, thread `s` aggregates all records in shard `s` using an open-addressed flat hash table. Latencies per group are sorted in-place, extracting nearest-rank p50, p95, and p99 percentiles and error PPM.
4. **Final Ordered Write:** Summaries across all shards are merged, sorted by `(hour, endpoint)` byte order, and formatted into an output buffer written to `/out/summary.txt` in a single syscall.

## Dependencies

None (standard C++23, OpenMP, POSIX, and x86 intrinsics).

## With another 24 hours

With another 24 hours, we would explore custom AVX-512 line-vectorized parsers and radix-sorting latency distributions for high-volume endpoints.
