use std::fs::File;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::thread;

const NUM_THREADS: usize = 12; // Exact match for 6 P-cores / 12 threads (--cpuset-cpus="0-11")
const INF_DIST: u64 = (1u64 << 62) - 1;

// Raw Linux x86_64 FFI for zero-dependency mmap / madvise
extern "C" {
    fn mmap(
        addr: *mut std::ffi::c_void,
        len: usize,
        prot: i32,
        flags: i32,
        fd: i32,
        offset: i64,
    ) -> *mut std::ffi::c_void;
    fn munmap(addr: *mut std::ffi::c_void, len: usize) -> i32;
    fn madvise(addr: *mut std::ffi::c_void, len: usize, advice: i32) -> i32;
}

const PROT_READ: i32 = 1;
const MAP_SHARED: i32 = 1;
const MADV_SEQUENTIAL: i32 = 2;
const MADV_DONTNEED: i32 = 4;

struct MappedFile {
    ptr: *const u8,
    len: usize,
}

unsafe impl Send for MappedFile {}
unsafe impl Sync for MappedFile {}

impl MappedFile {
    fn open(path: &str) -> Self {
        let file = File::open(path).expect("Failed to open input file");
        let len = file.metadata().expect("Failed metadata").len() as usize;
        if len == 0 {
            return Self {
                ptr: std::ptr::null(),
                len: 0,
            };
        }
        let ptr = unsafe {
            let p = mmap(
                std::ptr::null_mut(),
                len,
                PROT_READ,
                MAP_SHARED,
                file.as_raw_fd(),
                0,
            );
            madvise(p, len, MADV_SEQUENTIAL);
            p as *const u8
        };
        Self { ptr, len }
    }

    #[inline(always)]
    fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    /// Evicts a page-aligned slice from the cgroup v2 memory charge immediately after parsing
    #[inline(always)]
    fn evict_range(&self, start: usize, end: usize) {
        let page_start = start & !4095usize;
        let page_end = end & !4095usize;
        if page_end > page_start {
            unsafe {
                madvise(
                    self.ptr.add(page_start) as *mut std::ffi::c_void,
                    page_end - page_start,
                    MADV_DONTNEED,
                );
            }
        }
    }
}

impl Drop for MappedFile {
    fn drop(&mut self) {
        if self.len > 0 {
            unsafe {
                madvise(self.ptr as *mut std::ffi::c_void, self.len, MADV_DONTNEED);
                munmap(self.ptr as *mut std::ffi::c_void, self.len);
            }
        }
    }
}

#[inline(always)]
fn parse_u64_at(bytes: &[u8], pos: &mut usize) -> u64 {
    let len = bytes.len();
    while *pos < len && unsafe { *bytes.get_unchecked(*pos) } <= b' ' {
        *pos += 1;
    }
    let mut val = 0u64;
    while *pos < len {
        let b = unsafe { *bytes.get_unchecked(*pos) };
        if b <= b' ' {
            *pos += 1;
            break;
        }
        val = val * 10 + (b - b'0') as u64;
        *pos += 1;
    }
    val
}

/// Edge packed into 64 bits: upper 32 bits = neighbor ID, lower 32 bits = weight.
/// Sorting a u64 slice sorts by neighbor first, then by weight!
type PackedEdge = u64;

#[inline(always)]
fn pack_edge(to: u32, weight: u32) -> PackedEdge {
    ((to as u64) << 32) | (weight as u64)
}

#[inline(always)]
fn unpack_edge(e: PackedEdge) -> (usize, u64) {
    ((e >> 32) as usize, (e & 0xFFFF_FFFF) as u64)
}

struct CsrGraph {
    n: usize,
    fwd_head: Vec<u32>,
    fwd_edges: Vec<PackedEdge>,
    bwd_head: Vec<u32>,
    bwd_edges: Vec<PackedEdge>,
}

impl CsrGraph {
    fn build_parallel(path: &str) -> Self {
        let mapped = MappedFile::open(path);
        let bytes = mapped.as_slice();

        let mut header_pos = 0usize;
        let n = parse_u64_at(bytes, &mut header_pos) as usize;
        let _m = parse_u64_at(bytes, &mut header_pos) as usize;

        // Align header_pos to beginning of first edge line
        while header_pos > 0 && bytes[header_pos - 1] != b'\n' {
            header_pos += 1;
        }

        // Compute 12 newline-aligned byte ranges across the file
        let body_len = bytes.len().saturating_sub(header_pos);
        let mut bounds = [0usize; NUM_THREADS + 1];
        bounds[0] = header_pos;
        bounds[NUM_THREADS] = bytes.len();
        for t in 1..NUM_THREADS {
            let mut p = header_pos + (body_len * t) / NUM_THREADS;
            while p < bytes.len() && bytes[p] != b'\n' {
                p += 1;
            }
            if p < bytes.len() {
                p += 1;
            }
            bounds[t] = p;
        }

        let fwd_deg: Vec<AtomicU32> = (0..=n).map(|_| AtomicU32::new(0)).collect();
        let bwd_deg: Vec<AtomicU32> = (0..=n).map(|_| AtomicU32::new(0)).collect();

        // PASS 1: 12-Thread Parallel Degree Counting + Immediate Page Cache Eviction
        thread::scope(|s| {
            for t in 0..NUM_THREADS {
                let start = bounds[t];
                let end = bounds[t + 1];
                let f_ref = &fwd_deg;
                let b_ref = &bwd_deg;
                let m_ref = &mapped;

                s.spawn(move || {
                    let slice = m_ref.as_slice();
                    let mut pos = start;
                    let mut last_evict = start;

                    while pos < end {
                        let u = parse_u64_at(slice, &mut pos) as usize;
                        if pos >= end && u == 0 {
                            break;
                        }
                        let v = parse_u64_at(slice, &mut pos) as usize;
                        let _w = parse_u64_at(slice, &mut pos);

                        if u != v && u < n && v < n {
                            unsafe {
                                f_ref.get_unchecked(u).fetch_add(1, Ordering::Relaxed);
                                b_ref.get_unchecked(v).fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        if pos - last_evict >= (1 << 20) {
                            m_ref.evict_range(last_evict, pos);
                            last_evict = pos;
                        }
                    }
                    m_ref.evict_range(last_evict, end);
                });
            }
        });

        // Prefix sum in-place over AtomicU32 arrays
        let mut sum_f = 0u32;
        let mut sum_b = 0u32;
        for i in 0..=n {
            let df = fwd_deg[i].load(Ordering::Relaxed);
            fwd_deg[i].store(sum_f, Ordering::Relaxed);
            sum_f += df;

            let db = bwd_deg[i].load(Ordering::Relaxed);
            bwd_deg[i].store(sum_b, Ordering::Relaxed);
            sum_b += db;
        }

        let valid_edges = sum_f as usize;
        let mut fwd_edges: Vec<PackedEdge> = Vec::with_capacity(valid_edges);
        let mut bwd_edges: Vec<PackedEdge> = Vec::with_capacity(valid_edges);
        unsafe {
            fwd_edges.set_len(valid_edges);
            bwd_edges.set_len(valid_edges);
        }

        let fwd_ptr = fwd_edges.as_mut_ptr() as usize;
        let bwd_ptr = bwd_edges.as_mut_ptr() as usize;

        // PASS 2: 12-Thread Lock-Free CSR Population + Immediate Page Cache Eviction
        thread::scope(|s| {
            for t in 0..NUM_THREADS {
                let start = bounds[t];
                let end = bounds[t + 1];
                let f_cursor = &fwd_deg;
                let b_cursor = &bwd_deg;
                let m_ref = &mapped;

                s.spawn(move || {
                    let slice = m_ref.as_slice();
                    let mut pos = start;
                    let mut last_evict = start;

                    while pos < end {
                        let u = parse_u64_at(slice, &mut pos) as usize;
                        if pos >= end && u == 0 {
                            break;
                        }
                        let v = parse_u64_at(slice, &mut pos) as usize;
                        let w = parse_u64_at(slice, &mut pos) as u32;

                        if u != v && u < n && v < n {
                            unsafe {
                                let fi =
                                    f_cursor.get_unchecked(u).fetch_add(1, Ordering::Relaxed) as usize;
                                *(fwd_ptr as *mut PackedEdge).add(fi) = pack_edge(v as u32, w);

                                let bi =
                                    b_cursor.get_unchecked(v).fetch_add(1, Ordering::Relaxed) as usize;
                                *(bwd_ptr as *mut PackedEdge).add(bi) = pack_edge(u as u32, w);
                            }
                        }
                        if pos - last_evict >= (1 << 20) {
                            m_ref.evict_range(last_evict, pos);
                            last_evict = pos;
                        }
                    }
                    m_ref.evict_range(last_evict, end);
                });
            }
        });

        drop(mapped);

        // Convert AtomicU32 cursors back into standard CSR offset vectors (shift right by 1)
        let mut fwd_head: Vec<u32> = unsafe { std::mem::transmute(fwd_deg) };
        let mut bwd_head: Vec<u32> = unsafe { std::mem::transmute(bwd_deg) };
        for i in (1..=n).rev() {
            fwd_head[i] = fwd_head[i - 1];
            bwd_head[i] = bwd_head[i - 1];
        }
        fwd_head[0] = 0;
        bwd_head[0] = 0;

        // PASS 3: Parallel Sorting of Adjacency Slices & In-Place Parallel Edge Compaction
        Self::parallel_sort_and_compact(n, &mut fwd_head, &mut fwd_edges);
        Self::parallel_sort_and_compact(n, &mut bwd_head, &mut bwd_edges);

        Self {
            n,
            fwd_head,
            fwd_edges,
            bwd_head,
            bwd_edges,
        }
    }

    fn parallel_sort_and_compact(n: usize, head: &mut [u32], edges: &mut Vec<PackedEdge>) {
        let edges_ptr = edges.as_mut_ptr() as usize;
        let head_ref = &*head;
        let next_node = AtomicUsize::new(0);

        thread::scope(|s| {
            for _ in 0..NUM_THREADS {
                let next_ref = &next_node;
                s.spawn(move || loop {
                    let base = next_ref.fetch_add(1024, Ordering::Relaxed);
                    if base >= n {
                        break;
                    }
                    let limit = (base + 1024).min(n);
                    for u in base..limit {
                        let start = unsafe { *head_ref.get_unchecked(u) as usize };
                        let end = unsafe { *head_ref.get_unchecked(u + 1) as usize };
                        if end - start > 1 {
                            let slice = unsafe {
                                std::slice::from_raw_parts_mut(
                                    (edges_ptr as *mut PackedEdge).add(start),
                                    end - start,
                                )
                            };
                            slice.sort_unstable();
                        }
                    }
                });
            }
        });

        // Fast linear compaction to drop parallel edges with higher weights
        let mut write_ptr = 0usize;
        let mut prev_start = 0usize;
        for u in 0..n {
            let end = unsafe { *head.get_unchecked(u + 1) as usize };
            let new_start = write_ptr;
            let mut last_to = u32::MAX;
            for idx in prev_start..end {
                let e = unsafe { *edges.get_unchecked(idx) };
                let to = (e >> 32) as u32;
                if to != last_to {
                    last_to = to;
                    unsafe {
                        *edges.get_unchecked_mut(write_ptr) = e;
                    }
                    write_ptr += 1;
                }
            }
            unsafe {
                *head.get_unchecked_mut(u) = new_start as u32;
            }
            prev_start = end;
        }
        unsafe {
            *head.get_unchecked_mut(n) = write_ptr as u32;
        }
        edges.truncate(write_ptr);
    }
}

/// 65-Bucket Monotone Radix Heap leveraging Raptor Lake LZCNT/TZCNT instructions
struct RadixHeap {
    buckets: [Vec<(u64, u32)>; 65],
    buckets_min: [u64; 65],
    last_deleted: u64,
    size: usize,
    non_empty_mask: u128,
}

impl RadixHeap {
    fn new() -> Self {
        Self {
            buckets: std::array::from_fn(|_| Vec::with_capacity(64)),
            buckets_min: [u64::MAX; 65],
            last_deleted: 0,
            size: 0,
            non_empty_mask: 0,
        }
    }

    #[inline(always)]
    fn clear(&mut self) {
        let mut mask = self.non_empty_mask;
        while mask != 0 {
            let idx = mask.trailing_zeros() as usize;
            unsafe {
                self.buckets.get_unchecked_mut(idx).clear();
                *self.buckets_min.get_unchecked_mut(idx) = u64::MAX;
            }
            mask &= mask - 1;
        }
        self.last_deleted = 0;
        self.size = 0;
        self.non_empty_mask = 0;
    }

    #[inline(always)]
    fn bucket_idx(key: u64, last: u64) -> usize {
        (64 - (key ^ last).leading_zeros()) as usize
    }

    #[inline(always)]
    fn push(&mut self, key: u64, val: u32) {
        self.size += 1;
        let idx = Self::bucket_idx(key, self.last_deleted);
        unsafe {
            self.buckets.get_unchecked_mut(idx).push((key, val));
            if key < *self.buckets_min.get_unchecked(idx) {
                *self.buckets_min.get_unchecked_mut(idx) = key;
            }
        }
        self.non_empty_mask |= 1u128 << idx;
    }

    #[inline(always)]
    fn pop(&mut self) -> Option<(u64, u32)> {
        if self.size == 0 {
            return None;
        }
        if (self.non_empty_mask & 1) == 0 {
            let idx = self.non_empty_mask.trailing_zeros() as usize;
            self.last_deleted = unsafe { *self.buckets_min.get_unchecked(idx) };

            let mut temp = std::mem::take(unsafe { self.buckets.get_unchecked_mut(idx) });
            unsafe {
                *self.buckets_min.get_unchecked_mut(idx) = u64::MAX;
            }
            self.non_empty_mask &= !(1u128 << idx);

            for &(k, v) in &temp {
                let new_idx = Self::bucket_idx(k, self.last_deleted);
                unsafe {
                    self.buckets.get_unchecked_mut(new_idx).push((k, v));
                    if k < *self.buckets_min.get_unchecked(new_idx) {
                        *self.buckets_min.get_unchecked_mut(new_idx) = k;
                    }
                }
                self.non_empty_mask |= 1u128 << new_idx;
            }
            temp.clear();
            unsafe {
                *self.buckets.get_unchecked_mut(idx) = temp;
            }
        }

        self.size -= 1;
        let b0 = unsafe { self.buckets.get_unchecked_mut(0) };
        let res = b0.pop().unwrap();
        if b0.is_empty() {
            unsafe {
                *self.buckets_min.get_unchecked_mut(0) = u64::MAX;
            }
            self.non_empty_mask &= !1u128;
        }
        Some(res)
    }

    #[inline(always)]
    fn top_key(&self) -> u64 {
        if self.size == 0 {
            return INF_DIST;
        }
        let idx = self.non_empty_mask.trailing_zeros() as usize;
        if idx == 0 {
            self.last_deleted
        } else {
            unsafe { *self.buckets_min.get_unchecked(idx) }
        }
    }
}

/// Ultra-compact per-thread solver state: only 2 u64 vectors per thread (16 bytes * N).
/// Dirty lists track touched vertices so cleanup takes O(touched) time.
struct BiDijkstraWorker {
    dist_f: Vec<u64>,
    dist_b: Vec<u64>,
    dirty_f: Vec<u32>,
    dirty_b: Vec<u32>,
    pq_f: RadixHeap,
    pq_b: RadixHeap,
}

impl BiDijkstraWorker {
    fn new(n: usize) -> Self {
        Self {
            dist_f: vec![INF_DIST; n],
            dist_b: vec![INF_DIST; n],
            dirty_f: Vec::with_capacity(4096),
            dirty_b: Vec::with_capacity(4096),
            pq_f: RadixHeap::new(),
            pq_b: RadixHeap::new(),
        }
    }

    #[inline(always)]
    fn solve(&mut self, g: &CsrGraph, s: usize, t: usize) -> i64 {
        if s == t {
            return 0;
        }
        if s >= g.n || t >= g.n {
            return -1;
        }
        unsafe {
            if *g.fwd_head.get_unchecked(s) == *g.fwd_head.get_unchecked(s + 1)
                || *g.bwd_head.get_unchecked(t) == *g.bwd_head.get_unchecked(t + 1)
            {
                return -1;
            }
        }

        self.pq_f.clear();
        self.pq_b.clear();

        unsafe {
            *self.dist_f.get_unchecked_mut(s) = 0;
            *self.dist_b.get_unchecked_mut(t) = 0;
        }
        self.dirty_f.push(s as u32);
        self.dirty_b.push(t as u32);

        self.pq_f.push(0, s as u32);
        self.pq_b.push(0, t as u32);

        let mut mu = INF_DIST;
        let mut work_f = 0usize;
        let mut work_b = 0usize;

        while self.pq_f.size > 0 && self.pq_b.size > 0 {
            if self.pq_f.top_key() + self.pq_b.top_key() >= mu {
                break;
            }

            if work_f <= work_b {
                let (d, u_u32) = self.pq_f.pop().unwrap();
                let u = u_u32 as usize;
                if d > unsafe { *self.dist_f.get_unchecked(u) } {
                    continue;
                }
                let start = unsafe { *g.fwd_head.get_unchecked(u) as usize };
                let end = unsafe { *g.fwd_head.get_unchecked(u + 1) as usize };
                work_f += end - start;

                for idx in start..end {
                    let (v, w) = unpack_edge(unsafe { *g.fwd_edges.get_unchecked(idx) });
                    let nd = d + w;
                    unsafe {
                        let df = self.dist_f.get_unchecked_mut(v);
                        if nd < *df {
                            if *df == INF_DIST {
                                self.dirty_f.push(v as u32);
                            }
                            *df = nd;
                            self.pq_f.push(nd, v as u32);
                            let db = *self.dist_b.get_unchecked(v);
                            if db != INF_DIST && nd + db < mu {
                                mu = nd + db;
                            }
                        }
                    }
                }
            } else {
                let (d, u_u32) = self.pq_b.pop().unwrap();
                let u = u_u32 as usize;
                if d > unsafe { *self.dist_b.get_unchecked(u) } {
                    continue;
                }
                let start = unsafe { *g.bwd_head.get_unchecked(u) as usize };
                let end = unsafe { *g.bwd_head.get_unchecked(u + 1) as usize };
                work_b += end - start;

                for idx in start..end {
                    let (v, w) = unpack_edge(unsafe { *g.bwd_edges.get_unchecked(idx) });
                    let nd = d + w;
                    unsafe {
                        let db = self.dist_b.get_unchecked_mut(v);
                        if nd < *db {
                            if *db == INF_DIST {
                                self.dirty_b.push(v as u32);
                            }
                            *db = nd;
                            self.pq_b.push(nd, v as u32);
                            let df = *self.dist_f.get_unchecked(v);
                            if df != INF_DIST && nd + df < mu {
                                mu = nd + df;
                            }
                        }
                    }
                }
            }
        }

        // Fast sparse reset of only touched vertices
        for &u in &self.dirty_f {
            unsafe {
                *self.dist_f.get_unchecked_mut(u as usize) = INF_DIST;
            }
        }
        self.dirty_f.clear();

        for &u in &self.dirty_b {
            unsafe {
                *self.dist_b.get_unchecked_mut(u as usize) = INF_DIST;
            }
        }
        self.dirty_b.clear();

        if mu == INF_DIST {
            -1
        } else {
            mu as i64
        }
    }
}

fn resolve_path(candidates: &[&str]) -> String {
    for &c in candidates {
        if std::path::Path::new(c).exists() {
            return c.to_string();
        }
    }
    candidates[0].to_string()
}

fn main() {
    let graph_path = resolve_path(&["/data/graph.txt", "graph.txt", "/in/graph.txt"]);
    let queries_path = resolve_path(&["/data/queries.txt", "queries.txt", "/in/queries.txt"]);
    let out_path = if std::path::Path::new("/out").exists() {
        "/out/distances.txt"
    } else {
        "distances.txt"
    };

    // 1. Parallel 12-Thread Build of Forward & Backward CSR Graph
    let graph = CsrGraph::build_parallel(&graph_path);

    // 2. Parse Queries via mmap
    let q_map = MappedFile::open(&queries_path);
    let q_bytes = q_map.as_slice();
    let mut q_pos = 0usize;
    let mut queries = Vec::with_capacity(2048);
    while q_pos < q_bytes.len() {
        while q_pos < q_bytes.len() && q_bytes[q_pos] <= b' ' {
            q_pos += 1;
        }
        if q_pos >= q_bytes.len() {
            break;
        }
        let s = parse_u64_at(q_bytes, &mut q_pos) as usize;
        let t = parse_u64_at(q_bytes, &mut q_pos) as usize;
        queries.push((s, t));
    }
    drop(q_map);

    let num_queries = queries.len();
    let mut results = vec![0i64; num_queries];

    // Dynamically cap worker count only if N is huge, so 50 MB RAM is never breached
    let worker_bytes = graph.n * 16; // 16 bytes per node per thread
    let active_threads = if worker_bytes > 0 {
        ((6 * 1024 * 1024) / worker_bytes).clamp(1, NUM_THREADS)
    } else {
        NUM_THREADS
    };

    let next_q = AtomicUsize::new(0);
    let results_ptr = results.as_mut_ptr() as usize;

    // 3. Execute Queries across P-Core Threads with Dynamic Work Stealing
    thread::scope(|s| {
        for _ in 0..active_threads {
            let g_ref = &graph;
            let q_ref = &queries;
            let next_ref = &next_q;

            s.spawn(move || {
                let mut worker = BiDijkstraWorker::new(g_ref.n);
                loop {
                    let base = next_ref.fetch_add(4, Ordering::Relaxed);
                    if base >= num_queries {
                        break;
                    }
                    let end = (base + 4).min(num_queries);
                    for i in base..end {
                        let (src, dst) = unsafe { *q_ref.get_unchecked(i) };
                        let ans = worker.solve(g_ref, src, dst);
                        unsafe {
                            *(results_ptr as *mut i64).add(i) = ans;
                        }
                    }
                }
            });
        }
    });

    // 4. Single-Syscall Buffered Output
    let mut out_file = File::create(out_path).expect("Failed to open output file");
    let mut out_buf = Vec::with_capacity(num_queries * 12);
    let mut itoa_buf = [0u8; 24];

    for &ans in &results {
        if ans == -1 {
            out_buf.extend_from_slice(b"-1\n");
        } else if ans == 0 {
            out_buf.extend_from_slice(b"0\n");
        } else {
            let mut val = ans as u64;
            let mut idx = 24;
            while val > 0 {
                idx -= 1;
                itoa_buf[idx] = b'0' + (val % 10) as u8;
                val /= 10;
            }
            out_buf.extend_from_slice(&itoa_buf[idx..24]);
            out_buf.push(b'\n');
        }
    }
    out_file.write_all(&out_buf).expect("Failed to write output");
}
