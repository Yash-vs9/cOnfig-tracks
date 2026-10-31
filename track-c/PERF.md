# Track C: performance log

Every step from the reference to your final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way you say it did.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | | 20.64 | 367 | 0 |
| 1 | C++23 zero-copy mmap, quote parity chunking, direct 1M LUT, parallel sharded dedup | run-2dc91c0fb143 | 0.111 | 57 | 60.74 |
| 2 | Unrolled branchless quote-scanning for name/note fields, pre-reserved exact shard capacities | run-90413a86a349 | 0.107 | 44 | 61.63 |
| 3 | Dense 0..240 country mapping, lock-free 262k-slot 256-bit bitmask hash table, zero vector sort | run-6b01d64cd47a | 0.064 | 15 | 70.53 |
| 4 | AVX2 quote counting, 16-byte SSE delimiter scan, L1 name filter, -march=native | run-78b3f05b5baa | 0.0609 | 14.0 | 70.76 |
