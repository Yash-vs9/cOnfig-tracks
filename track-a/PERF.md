# Track A: performance log

Every step from the reference to your final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way you say it did.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | | 32.05 | 228 | 0 |
| 1 | C++23 zero-copy mmap, 12-thread two-stage lock-free sharded aggregation | run-f4217bdda361 | 0.141 | 92 | 61.72 |
| 2 | Packed 12-byte record alignment, pre-reserved non-reallocating buckets, immediate swap deallocation | run-6f38cca425c8 | 0.141 | 59 | 63.10 |
| 3 | Static BSS contiguous latency slab, 64-bit hardware CRC32 endpoint hashing, tuned bucket reservation | run-a5931f08ba3e | 0.131 | 62 | 63.46 |
| 4 | Parallel 72-hour bucket sorting, concurrent 12-thread buffer formatting, integer hour ID matching | run-9d3f72b2c8e5 | 0.0697 | 62.9 | 69.60 |
| 5 | Zero-dependency Rust lock-free block arena, SIMD CRC32, parallel CSR, pdqselect select_nth_unstable | | | | |
