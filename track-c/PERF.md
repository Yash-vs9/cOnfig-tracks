# Track C: performance log

Every step from the reference to your final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way you say it did.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | | 20.64 | 367 | 0 |
| 1 | C++23 zero-copy mmap, quote parity chunking, direct 1M LUT, parallel sharded dedup | run-2dc91c0fb143 | 0.111 | 57 | 60.74 |
| 2 | Unrolled branchless quote-scanning for name/note fields, pre-reserved exact shard capacities | run-90413a86a349 | 0.107 | 44 | 61.63 |
| 3 | AVX2 vectorized quote-parity scan, hardware CRC32 name hashing, tuned capacity and swap dealloc | | | | |
