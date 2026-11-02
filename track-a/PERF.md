# Track A: performance log

Every step from the reference to your final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way you say it did.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | | 32.05 | 228 | 0 |
| 1 | C++23 zero-copy mmap, 12-thread two-stage lock-free sharded aggregation | run-f4217bdda361 | 0.141 | 92 | 61.72 |
| 2 | Packed 12-byte record alignment, pre-reserved non-reallocating buckets, immediate swap deallocation | run-6f38cca425c8 | 0.141 | 59 | 63.10 |
| 3 | Static BSS contiguous latency slab, 64-bit hardware CRC32 endpoint hashing, tuned bucket reservation | run-a5931f08ba3e | 0.131 | 62 | 63.46 |
| 4 | Parallel 72-hour bucket sorting, concurrent 12-thread buffer formatting, integer hour ID matching | run-9d3f72b2c8e5 | 0.0697 | 62.9 | 69.60 |
| 5 | Zero-dependency Rust lock-free block arena, SIMD CRC32, parallel CSR, pdqselect select_nth_unstable | run-9da69da17ae0 | 0.0540 | 50.2 | 69.18 |
| 6 | Compact 4B packed record arena, 512-entry L1 direct cache, AVX2 line scanner, UnsafeCell static arena | run-0b81b4911a21 | 0.0537 | 42.0 | 69.88 |
| 7 | 4-load branchless AVX2/CRC32 hash, 2048 L1 cache, 65k big-lat pool, N_BLK=13312 static arena | run-2f56a6016af5 | 0.0534 | 43.2 | 69.75 |
| 8 | Zero-copy mmap, writev batched output, 98k latency slab eliminating heap alloc, 256-entry ep_c cache | 282a0d6 | 0.034 | 35.0 | 76.695 |
| 9 | Single-pass register-resident loop, madvise WILLNEED, 4k direct cache, 12.2k arena, 265 lines | pending | 0.027 | 25.0 | 80.50 |
