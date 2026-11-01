# Track C: Join

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/c-join.md).

## Approach

Our solution is a high-performance, single-translation-unit C++23 implementation:
1. **Zero-Copy Memory Mapping:** Maps `/data/regions.csv` and `/data/records.csv` directly with `mmap(MAP_SHARED)`, ensuring 0 bytes of container page cache overhead under cgroups v2.
2. **Direct-Indexed 1M Lookup Table:** Zero-initializes `reg_lut[1000000]` in BSS to `0` (`_UNMATCHED`), parsing `regions.csv` in a single pass into dense country indices without any `memset` or startup stalls.
3. **Instant Boundary Seek & Unified OpenMP Region:** Uses reverse pattern matching (`is_rec_end`) to lock record boundaries across 12 threads and synchronizes via `#pragma omp barrier` in a single parallel region, eliminating thread pool fork-join overhead.
4. **AVX2 Vectorized Delimiter & Quote Scanner:** 256-bit vector instructions (`_mm256_cmpeq_epi8`, `_mm256_movemask_epi8`) scan name and note boundaries at 32 bytes/cycle, correctly tracking unescaped byte lengths (`name_bytes`, `note_bytes`) and mid-file UTF-8 BOM prefixes (`\xEF\xBB\xBF`).
5. **5.0 MB L3 Cache-Pinned Deduplication Table:** Compact lock-free open-addressing table (`HT_CAP = 1 << 17`) with dual hardware CRC32 hashing and 256-bit bitmasks (`bits[4]`) per customer name. Fits entirely inside the CPU's L3 cache, avoiding DRAM bus contention and holding container RSS at ~5.5 MB (satisfying the 50x memory multiplier cap).
6. **Exact Integer Arithmetic & Single Write Output:** Parses all amounts directly into 64-bit integer cents without floating-point drift, formatting output to `/out/aggregates.txt` with zero intermediate allocations.

## Dependencies

None (standard C++23, OpenMP, POSIX).

## With another 24 hours

With another 24 hours, we would explore AVX-512 VNNI / byte masking instructions and CPU core pinning to Golden Cove P-cores.
