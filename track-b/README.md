# Track B: Index

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/b-index.md).

## Approach

Our solution is a zero-dependency C++20 engine engineered to maximize the Speed Demon scoring formula $K = (T_{\text{ref}}/T)^{0.50} \times \min(M_{\text{ref}}/M, 50)^{0.20} \times \min(S_{\text{ref}}/S, 1)^{0.15} \times (D_{\text{ref}}/D)^{0.15}$.

### 1. Zero-Copy I/O & Query-Filtered Vocabulary
- Both `/data/docs.txt` and `/data/queries.txt` are memory-mapped using `mmap(PROT_READ, MAP_SHARED)`. Since `/data` is pre-warmed in the host page cache via `vmtouch`, reading documents and queries produces zero dirty page allocations and zero cost in cgroup v2 `memory.peak`.
- We parse `queries.txt` first to build a fixed hash dictionary of only the ~75.7k distinct terms that appear in queries. Any corpus words not present in the query vocabulary advance token positions for phrase evaluation but require zero posting memory allocations.

### 2. Hybrid Compact Posting Storage ($M \le 44.5\text{ MB}$, 50x Cap Hit)
- Documents are split into 12 fixed chunks ($\le 20,834$ docs per chunk, aligned to 64 documents). Because each chunk has $\le 21,000$ documents, document IDs within any chunk fit into a compact 2-byte `uint16_t` ($d - \text{chunk\_start} < 65,536$).
- **Mathematical Breakeven:** A full 250k-bit bitset across all documents requires $250,000 / 8 = 31,250$ bytes. Storing postings as 2-byte `uint16_t` requires $2 \times df$ bytes. Therefore, when $df > 15,625$, bitsets are more memory-efficient than posting lists.
- Terms with $df > 15,625$ (the top ~180 terms) are indexed as global 64-bit aligned bitsets (~5.6 MB).
- The remaining terms are stored in a contiguous 12-chunk Compressed Sparse Row (CSR) array of `uint16_t` postings (~28.4 MB).
- Document ingestion runs in two parallel passes:
  - **Pass 1:** 12 worker threads count term frequencies per chunk into temporary anonymous `mmap` buffers.
  - **Reallocation Guard:** Pass 1 counting tables are immediately `munmap`ed before allocating the final global posting arrays, keeping the high-water mark below 45 MB.
  - **Pass 2:** Threads populate the bitsets and CSR arrays concurrently without lock contention.
- This hybrid design caps peak anonymous memory at **44.53 MB**, hitting the maximum 50x memory multiplier ($50^{0.20} = 2.1867\times$).

### 3. Option B for Phrase Queries (Spec Compliant)
- In accordance with Section 3 and Section 8 of `specs/b-index.md`, we avoid allocating a 100 MB positional posting index.
- Phrase queries sort their distinct terms by ascending document frequency ($df$) and intersect the document lists starting with the rarest anchor term.
- Candidate documents are verified on-the-fly directly against the `mmap` document text slice, tokenizing the candidate text and matching exact token sequences.
- **Repeat Phrase Protection (`"w w"`):** For the ~450 terms that ever appear repeated in a phrase query, Pass 1 and Pass 2 track consecutive occurrences (`pos == prev_pos + 1`) into a tiny dedicated list (< 200 KB), collapsing candidate document counts even for phrases with repeated high-frequency words.

### 4. Lock-Free Parallel Query Execution & Fast Output
- 12 worker threads execute queries in parallel using lock-free atomic ticket batches (64 queries per ticket).
- Boolean queries evaluate bitsets directly on the thread's stack (384 words = 3 KB) with zero heap allocation.
- Query results are formatted via `std::to_chars` into a preallocated 1.5 MB results buffer and written to `/out/results.txt` via 64 KB buffered POSIX `write()`.

## Dependencies

None. Zero non-standard dependencies ($D = 1.0$), using pure standard C++20 and POSIX system calls (`mmap`, `write`).

## With another 24 hours

1. **AVX2 SIMD Vectorization:** Utilize AVX2 256-bit vector intrinsics (`_mm256_and_si256`, `_mm256_or_si256`) for boolean chunk bitset evaluation.
2. **SIMD Token Lexing:** Accelerate ASCII lowercasing and separator scanning during document parsing using SIMD range checks (`_mm256_cmpeq_epi8` / `_mm256_cmpgt_epi8`).
3. **Adaptive Intersect Short-Circuiting:** Skip candidate token verification if previous token positions can be proven impossible using chunk-level bounds.
