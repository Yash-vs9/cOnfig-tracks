# Track C: Join

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/c-join.md).

## Approach

Our solution is a high-performance, single-translation-unit C++23 implementation targeting <= 100 lines:
1. **Zero-Copy Memory Mapping:** Maps `/data/regions.csv` and `/data/records.csv` directly with `mmap(MAP_SHARED)`, avoiding userspace memory copy overhead.
2. **Direct-Indexed 1M Lookup Table:** Zero-initializes `reg_lut[1000000]` in BSS to `0` (`_UNMATCHED`), parsing `regions.csv` into 3-letter country indices (`1..17576` for `AAA..ZZZ`) for $O(1)$ lock-free join lookups without hash overhead.
3. **Two-Pass Quote-Parity Parallel Chunking:** Divides `records.csv` into 12 slices. Pass 1 counts quote bytes in parallel; prefix-XOR determines quote state at each slice start, resolving exact record boundaries even across embedded newlines.
4. **Streaming Scanner & Hostile CSV Handling:** Strips mid-file and header UTF-8 BOM (`\xEF\xBB\xBF`), handles escaped `""` and unescaped byte length counting (`name_bytes`, `note_bytes`), and parses amounts into exact signed 64-bit integer cents.
5. **Parallel Lock-Free Deduplication:** Records are routed to 12 thread shards via `cid % 12`. Each thread independently sorts and deduplicates distinct names per country in parallel before final ordered emission to `/out/aggregates.txt`.

## Dependencies

None (standard C++23, OpenMP, POSIX).

## With another 24 hours

With another 24 hours, we would implement AVX2/AVX-512 bitmask quote scanners and custom cache-friendly Robin Hood hash tables for in-flight distinct-name deduplication.
