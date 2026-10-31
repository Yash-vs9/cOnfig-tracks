/**
 * Speed Demon 2026 - Track D (Traverse)
 * High-Performance Zero-Dependency Rust Engine
 * Batch-Atomic CSR + Dataset Bucketing + 1-Bit Sink Filter + 8-Ary Packed Cache-Line Heap
 */

use std::arch::x86_64::{_mm_prefetch, _MM_HINT_T0};
use std::fs::File;
use std::io::Write;
use std::os::unix::io::IntoRawFd;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

extern "C" {
    fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
    fn munmap(a: *mut u8, l: usize) -> i32;
    fn madvise(a: *mut u8, l: usize, adv: i32) -> i32;
    fn close(fd: i32) -> i32;
}

#[derive(Clone, Copy, Default)]
#[repr(C)]
struct Edge {
    to: u32,
    w: u32,
}

#[derive(Clone, Copy)]
struct Query {
    s: u32,
    t: u32,
    id: u32,
}

struct SourceGroup {
    s: u32,
    begin_idx: usize,
    end_idx: usize,
}

struct FastHeap8 {
    data: Vec<u64>,
}

impl FastHeap8 {
    #[inline(always)]
    fn with_capacity(cap: usize) -> Self {
        Self {
            data: Vec::with_capacity(cap),
        }
    }

    #[inline(always)]
    fn clear(&mut self) {
        self.data.clear();
    }

    #[inline(always)]
    fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    #[inline(always)]
    fn push(&mut self, packed: u64) {
        let mut i = self.data.len();
        self.data.push(packed);
        unsafe {
            let slice = self.data.as_mut_ptr();
            while i > 0 {
                let p = (i - 1) >> 3;
                if packed >= *slice.add(p) {
                    break;
                }
                *slice.add(i) = *slice.add(p);
                i = p;
            }
            *slice.add(i) = packed;
        }
    }

    #[inline(always)]
    fn pop(&mut self) -> u64 {
        unsafe {
            let top = *self.data.get_unchecked(0);
            let last = self.data.pop().unwrap_unchecked();
            let sz = self.data.len();
            if sz > 0 {
                let slice = self.data.as_mut_ptr();
                let mut i = 0;
                loop {
                    let c = (i << 3) + 1;
                    if c >= sz {
                        break;
                    }
                    let mut mc = c;
                    let ec = (c + 8).min(sz);
                    for k in c + 1..ec {
                        if *slice.add(k) < *slice.add(mc) {
                            mc = k;
                        }
                    }
                    if *slice.add(mc) >= last {
                        break;
                    }
                    *slice.add(i) = *slice.add(mc);
                    i = mc;
                }
                *slice.add(i) = last;
            }
            top
        }
    }
}

struct FastMmap {
    ptr: *mut u8,
    size: usize,
}

impl FastMmap {
    fn new(path: &str) -> Option<Self> {
        let f = File::open(path).ok()?;
        let meta = f.metadata().ok()?;
        let size = meta.len() as usize;
        if size == 0 {
            return None;
        }
        let fd = f.into_raw_fd();
        let ptr = unsafe {
            mmap(
                ptr::null_mut(),
                size,
                1, // PROT_READ
                2, // MAP_PRIVATE
                fd,
                0,
            )
        };
        unsafe { close(fd); }
        if ptr == usize::MAX as *mut u8 || ptr.is_null() {
            None
        } else {
            unsafe { madvise(ptr, size, 2); } // MADV_SEQUENTIAL
            Some(Self { ptr, size })
        }
    }

    #[inline(always)]
    fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.size) }
    }
}

impl Drop for FastMmap {
    fn drop(&mut self) {
        if !self.ptr.is_null() && self.ptr != usize::MAX as *mut u8 {
            unsafe { munmap(self.ptr, self.size); }
            self.ptr = ptr::null_mut();
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut data_dir = "/data".to_string();
    let mut out_dir = "/out".to_string();
    let mut i = 1;
    let mut pos = 0;
    while i < args.len() {
        if args[i] == "--data" && i + 1 < args.len() {
            data_dir = args[i + 1].clone();
            i += 2;
        } else if args[i] == "--out" && i + 1 < args.len() {
            out_dir = args[i + 1].clone();
            i += 2;
        } else if !args[i].starts_with('-') {
            if pos == 0 {
                data_dir = args[i].clone();
            } else {
                out_dir = args[i].clone();
            }
            pos += 1;
            i += 1;
        } else {
            i += 1;
        }
    }

    let gmap = FastMmap::new(&format!("{}/graph.txt", data_dir)).expect("Failed to open graph.txt");
    let g_bytes = gmap.as_slice();
    let mut g_end = g_bytes.len();
    while g_end > 0 && g_bytes[g_end - 1] < b'0' {
        g_end -= 1;
    }

    let mut p = 0;
    let mut n: u32 = 0;
    while p < g_end && g_bytes[p] >= b'0' {
        n = n * 10 + (g_bytes[p] - b'0') as u32;
        p += 1;
    }
    while p < g_end && g_bytes[p] < b'0' { p += 1; }
    let mut m: u32 = 0;
    while p < g_end && g_bytes[p] >= b'0' {
        m = m * 10 + (g_bytes[p] - b'0') as u32;
        p += 1;
    }
    while p < g_end && g_bytes[p] < b'0' { p += 1; }

    let edge_start = p;
    let is_tiny = m < 50_000;
    let is_sample = !is_tiny && m < 2_000_000;
    let nt = if is_tiny { 1 } else if is_sample { 6 } else { 12 };
    let dijkstra_workers = if is_tiny { 1 } else if is_sample { 6 } else { 8 };

    let edge_bytes = g_end - edge_start;
    let mut chunk_bounds = vec![g_end; nt + 1];
    chunk_bounds[0] = edge_start;
    for t in 1..nt {
        let mut cp = edge_start + (edge_bytes * t) / nt;
        while cp < g_end && g_bytes[cp - 1] != b'\n' {
            cp += 1;
        }
        chunk_bounds[t] = cp;
    }

    let head: Vec<AtomicU32> = (0..=n).map(|_| AtomicU32::new(0)).collect();
    let has_in_bitset: Vec<AtomicU64> = (0..((n as usize + 63) >> 6)).map(|_| AtomicU64::new(0)).collect();

    // Pass 1: Parallel degree counting with zero bounds-checked parsing
    let base_ptr = g_bytes.as_ptr() as usize;
    std::thread::scope(|scope| {
        for t in 0..nt {
            let cur_start = chunk_bounds[t];
            let cur_end = chunk_bounds[t + 1];
            let head_ref = &head;
            let bit_ref = &has_in_bitset;

            scope.spawn(move || {
                let mut cur = cur_start;
                let mut prev_u = u32::MAX;
                let mut run_cnt = 0u32;
                let bptr = base_ptr as *const u8;

                unsafe {
                    while cur < cur_end {
                        let mut u = (*bptr.add(cur) - b'0') as u32;
                        cur += 1;
                        while cur < cur_end && *bptr.add(cur) >= b'0' {
                            u = u * 10 + (*bptr.add(cur) - b'0') as u32;
                            cur += 1;
                        }
                        cur += 1;
                        let mut v = (*bptr.add(cur) - b'0') as u32;
                        cur += 1;
                        while cur < cur_end && *bptr.add(cur) >= b'0' {
                            v = v * 10 + (*bptr.add(cur) - b'0') as u32;
                            cur += 1;
                        }
                        cur += 1;
                        while cur < cur_end && *bptr.add(cur) >= b'0' {
                            cur += 1;
                        }
                        if cur < cur_end && *bptr.add(cur) == b'\r' {
                            cur += 1;
                        }
                        if cur < cur_end {
                            cur += 1;
                        }

                        if u != v {
                            if u == prev_u {
                                run_cnt += 1;
                            } else {
                                if run_cnt > 0 {
                                    head_ref.get_unchecked(prev_u as usize).fetch_add(run_cnt, Ordering::Relaxed);
                                }
                                prev_u = u;
                                run_cnt = 1;
                            }
                            let mask = 1u64 << (v & 63);
                            let word_idx = (v >> 6) as usize;
                            if bit_ref.get_unchecked(word_idx).load(Ordering::Relaxed) & mask == 0 {
                                bit_ref.get_unchecked(word_idx).fetch_or(mask, Ordering::Relaxed);
                            }
                        }
                    }
                    if run_cnt > 0 {
                        head_ref.get_unchecked(prev_u as usize).fetch_add(run_cnt, Ordering::Relaxed);
                    }
                }
            });
        }
    });

    let mut accum = 0u32;
    let head_pos: Vec<AtomicU32> = head.into_iter().map(|a| {
        let d = a.load(Ordering::Relaxed);
        let pos = accum;
        accum += d;
        AtomicU32::new(pos)
    }).collect();

    let mut edges = vec![Edge::default(); accum as usize];
    let edges_addr = edges.as_mut_ptr() as usize;

    // Pass 2: In-place CSR edge population
    std::thread::scope(|scope| {
        for t in 0..nt {
            let cur_start = chunk_bounds[t];
            let cur_end = chunk_bounds[t + 1];
            let pos_ref = &head_pos;

            scope.spawn(move || {
                let mut cur = cur_start;
                let mut prev_u = u32::MAX;
                let mut cnt = 0usize;
                let mut buf = [Edge::default(); 64];
                let bptr = base_ptr as *const u8;
                let eptr = edges_addr as *mut Edge;

                unsafe {
                    while cur < cur_end {
                        let mut u = (*bptr.add(cur) - b'0') as u32;
                        cur += 1;
                        while cur < cur_end && *bptr.add(cur) >= b'0' {
                            u = u * 10 + (*bptr.add(cur) - b'0') as u32;
                            cur += 1;
                        }
                        cur += 1;
                        let mut v = (*bptr.add(cur) - b'0') as u32;
                        cur += 1;
                        while cur < cur_end && *bptr.add(cur) >= b'0' {
                            v = v * 10 + (*bptr.add(cur) - b'0') as u32;
                            cur += 1;
                        }
                        cur += 1;
                        let mut w = (*bptr.add(cur) - b'0') as u32;
                        cur += 1;
                        while cur < cur_end && *bptr.add(cur) >= b'0' {
                            w = w * 10 + (*bptr.add(cur) - b'0') as u32;
                            cur += 1;
                        }
                        if cur < cur_end && *bptr.add(cur) == b'\r' {
                            cur += 1;
                        }
                        if cur < cur_end {
                            cur += 1;
                        }

                        if u != v {
                            if u == prev_u && cnt < 64 {
                                buf[cnt] = Edge { to: v, w };
                                cnt += 1;
                            } else {
                                if cnt > 0 {
                                    let idx = pos_ref.get_unchecked(prev_u as usize).fetch_add(cnt as u32, Ordering::Relaxed) as usize;
                                    std::ptr::copy_nonoverlapping(buf.as_ptr(), eptr.add(idx), cnt);
                                }
                                prev_u = u;
                                buf[0] = Edge { to: v, w };
                                cnt = 1;
                            }
                        }
                    }
                    if cnt > 0 {
                        let idx = pos_ref.get_unchecked(prev_u as usize).fetch_add(cnt as u32, Ordering::Relaxed) as usize;
                        std::ptr::copy_nonoverlapping(buf.as_ptr(), eptr.add(idx), cnt);
                    }
                }
            });
        }
    });

    drop(gmap);
    drop(chunk_bounds);

    let mut head: Vec<u32> = head_pos.into_iter().map(|a| a.into_inner()).collect();
    if n > 0 {
        head.copy_within(0..n as usize, 1);
    }
    head[0] = 0;

    // Fast register insertion-sort for small degrees + duplicate check
    let next_chunk = AtomicU32::new(0);
    let has_dups = AtomicBool::new(false);
    let csz = if is_tiny { 256 } else { 8192 };

    std::thread::scope(|scope| {
        for _ in 0..nt {
            let next_ref = &next_chunk;
            let dups_ref = &has_dups;
            let head_ref = &head;

            scope.spawn(move || {
                let mut local_dups = false;
                loop {
                    let u_base = next_ref.fetch_add(csz, Ordering::Relaxed);
                    if u_base >= n {
                        break;
                    }
                    let u_end = (u_base + csz).min(n);
                    for u in u_base..u_end {
                        let s = head_ref[u as usize] as usize;
                        let e = head_ref[u as usize + 1] as usize;
                        let len = e - s;
                        if len <= 1 {
                            continue;
                        }
                        let base = unsafe { (edges_addr as *mut Edge).add(s) };
                        if len <= 24 {
                            unsafe {
                                for i in 1..len {
                                    let key = *base.add(i);
                                    let mut j = i as isize - 1;
                                    while j >= 0 {
                                        let elem = *base.add(j as usize);
                                        if elem.to > key.to || (elem.to == key.to && elem.w > key.w) {
                                            *base.add((j + 1) as usize) = elem;
                                            j -= 1;
                                        } else {
                                            break;
                                        }
                                    }
                                    *base.add((j + 1) as usize) = key;
                                }
                            }
                        } else {
                            let slice = unsafe { std::slice::from_raw_parts_mut(base, len) };
                            slice.sort_unstable_by(|a, b| {
                                if a.to != b.to { a.to.cmp(&b.to) } else { a.w.cmp(&b.w) }
                            });
                        }
                        if !local_dups {
                            unsafe {
                                for i in 1..len {
                                    if (*base.add(i)).to == (*base.add(i - 1)).to {
                                        local_dups = true;
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
                if local_dups {
                    dups_ref.store(true, Ordering::Relaxed);
                }
            });
        }
    });

    if has_dups.load(Ordering::Relaxed) {
        let mut compacted = 0usize;
        for u in 0..n as usize {
            let s = head[u] as usize;
            let e = head[u + 1] as usize;
            let u_start = compacted;
            for i in s..e {
                if compacted > u_start && edges[i].to == edges[compacted - 1].to {
                    continue;
                }
                edges[compacted] = edges[i];
                compacted += 1;
            }
            head[u] = u_start as u32;
        }
        head[n as usize] = compacted as u32;
    }

    // Build 1-bit fast filter of vertices with out-degree > 0
    let mut num_sinks = 0u32;
    let mut has_out_bitset = vec![0u64; (n as usize + 63) >> 6];
    for u in 0..n as usize {
        if head[u] < head[u + 1] {
            has_out_bitset[u >> 6] |= 1u64 << (u & 63);
        } else {
            num_sinks += 1;
        }
    }
    let use_sink_filter = (num_sinks as u64 * 100) >= (n as u64);

    // Parse queries
    let qmap = FastMmap::new(&format!("{}/queries.txt", data_dir)).expect("Failed to open queries.txt");
    let q_bytes = qmap.as_slice();
    let q_end = q_bytes.len();
    let mut queries = Vec::with_capacity(4096);
    let mut qp = 0;
    let mut qid = 0u32;

    while qp < q_end {
        while qp < q_end && q_bytes[qp] < b'0' { qp += 1; }
        if qp >= q_end { break; }
        let mut s = 0u32;
        while qp < q_end && q_bytes[qp] >= b'0' {
            s = s * 10 + (q_bytes[qp] - b'0') as u32;
            qp += 1;
        }
        while qp < q_end && q_bytes[qp] < b'0' { qp += 1; }
        let mut t = 0u32;
        while qp < q_end && q_bytes[qp] >= b'0' {
            t = t * 10 + (q_bytes[qp] - b'0') as u32;
            qp += 1;
        }
        queries.push(Query { s, t, id: qid });
        qid += 1;
    }
    drop(qmap);

    let in_bits: Vec<u64> = has_in_bitset.into_iter().map(|a| a.into_inner()).collect();
    let mut answers = vec![-1i64; queries.len()];
    let mut active_q = Vec::with_capacity(queries.len());

    for q in queries {
        if q.s == q.t {
            answers[q.id as usize] = 0;
        } else if q.s < n && q.t < n && head[q.s as usize] < head[q.s as usize + 1]
            && (in_bits[(q.t >> 6) as usize] & (1u64 << (q.t & 63))) != 0
        {
            active_q.push(q);
        }
    }
    drop(in_bits);

    active_q.sort_unstable_by(|a, b| {
        if a.s != b.s { a.s.cmp(&b.s) } else { a.t.cmp(&b.t) }
    });

    let mut groups = Vec::new();
    let mut i = 0;
    let sz = active_q.len();
    while i < sz {
        let mut j = i + 1;
        while j < sz && active_q[j].s == active_q[i].s {
            j += 1;
        }
        groups.push(SourceGroup { s: active_q[i].s, begin_idx: i, end_idx: j });
        i = j;
    }
    groups.sort_unstable_by(|a, b| {
        (b.end_idx - b.begin_idx).cmp(&(a.end_idx - a.begin_idx))
    });

    let q_threads = 1.max(dijkstra_workers.min(groups.len()));
    let next_group = AtomicUsize::new(0);
    let dirty_limit = if is_tiny { n as usize } else { (n as usize >> 4) + 1024 };

    const V_BITS: u32 = 22;
    const V_MASK: u64 = (1u64 << V_BITS) - 1;

    let edge_addr = edges.as_ptr() as usize;
    let head_addr = head.as_ptr() as usize;
    let out_bit_addr = has_out_bitset.as_ptr() as usize;
    let answers_addr = answers.as_mut_ptr() as usize;

    std::thread::scope(|scope| {
        for _ in 0..q_threads {
            let next_ref = &next_group;
            let groups_ref = &groups;
            let active_ref = &active_q;

            scope.spawn(move || {
                let mut dist = vec![u64::MAX; n as usize];
                let mut is_target = vec![0u64; (n as usize + 63) >> 6];
                let mut dirty = Vec::with_capacity(dirty_limit);
                let mut heap = FastHeap8::with_capacity(if is_tiny { 512 } else { 32768 });

                let d_ptr = dist.as_mut_ptr();
                let head_ptr = head_addr as *const u32;
                let edge_ptr = edge_addr as *const Edge;
                let out_bit_ptr = out_bit_addr as *const u64;
                let answers_ptr = answers_addr as *mut i64;

                loop {
                    let gi = next_ref.fetch_add(1, Ordering::Relaxed);
                    if gi >= groups_ref.len() { break; }

                    let grp = &groups_ref[gi];
                    let s = grp.s as usize;
                    let single_t = active_ref[grp.begin_idx].t;
                    let is_single = single_t == active_ref[grp.end_idx - 1].t;

                    heap.clear();
                    dirty.clear();
                    let mut dirty_overflow = false;

                    unsafe {
                        *d_ptr.add(s) = 0;
                    }
                    dirty.push(s as u32);
                    heap.push(s as u64);

                    if is_single {
                        // Dedicated Single-Target Fast Path
                        let mut bound = u64::MAX;
                        while !heap.is_empty() {
                            let top = heap.pop();
                            let d = top >> V_BITS;
                            let u = (top & V_MASK) as usize;

                            if d >= bound { break; }
                            unsafe {
                                if d > *d_ptr.add(u) { continue; }

                                let e_start = *head_ptr.add(u) as usize;
                                let e_end = *head_ptr.add(u + 1) as usize;
                                for e in e_start..e_end {
                                    if e + 4 < e_end {
                                        let next_v = (*edge_ptr.add(e + 4)).to as usize;
                                        _mm_prefetch(d_ptr.add(next_v) as *const i8, _MM_HINT_T0);
                                    }
                                    let edge = *edge_ptr.add(e);
                                    let v = edge.to as usize;
                                    let nd = d + edge.w as u64;
                                    let cur_vd = *d_ptr.add(v);
                                    if nd < cur_vd && nd < bound {
                                        if cur_vd == u64::MAX && !dirty_overflow {
                                            if dirty.len() < dirty_limit {
                                                dirty.push(v as u32);
                                            } else {
                                                dirty_overflow = true;
                                            }
                                        }
                                        *d_ptr.add(v) = nd;
                                        if v == single_t as usize {
                                            bound = nd;
                                        } else if !use_sink_filter || ((*out_bit_ptr.add(v >> 6) & (1u64 << (v & 63))) != 0) {
                                            heap.push((nd << V_BITS) | (v as u64));
                                        }
                                    }
                                }
                            }
                        }

                        unsafe {
                            let final_d = *d_ptr.add(single_t as usize);
                            let ans_val = if final_d == u64::MAX { -1 } else { final_d as i64 };
                            for k in grp.begin_idx..grp.end_idx {
                                *answers_ptr.add(active_ref[k].id as usize) = ans_val;
                            }
                        }
                    } else {
                        // Multi-Target Group Path
                        let mut remaining = 0u32;
                        for k in grp.begin_idx..grp.end_idx {
                            let tk = active_ref[k].t as usize;
                            let mask = 1u64 << (tk & 63);
                            if is_target[tk >> 6] & mask == 0 {
                                is_target[tk >> 6] |= mask;
                                remaining += 1;
                            }
                        }

                        let mut bound = u64::MAX;
                        let mut curr_single_t = single_t;

                        while !heap.is_empty() {
                            let top = heap.pop();
                            let d = top >> V_BITS;
                            let u = (top & V_MASK) as usize;

                            if d >= bound { break; }
                            unsafe {
                                if d > *d_ptr.add(u) { continue; }

                                if remaining > 1 {
                                    let mask = 1u64 << (u & 63);
                                    if is_target[u >> 6] & mask != 0 {
                                        is_target[u >> 6] &= !mask;
                                        remaining -= 1;
                                        if remaining == 1 {
                                            for k in grp.begin_idx..grp.end_idx {
                                                let tk = active_ref[k].t as usize;
                                                if is_target[tk >> 6] & (1u64 << (tk & 63)) != 0 {
                                                    curr_single_t = tk as u32;
                                                    is_target[tk >> 6] &= !(1u64 << (tk & 63));
                                                    bound = *d_ptr.add(tk);
                                                    break;
                                                }
                                            }
                                            if d >= bound { break; }
                                        }
                                    }
                                }

                                let e_start = *head_ptr.add(u) as usize;
                                let e_end = *head_ptr.add(u + 1) as usize;
                                for e in e_start..e_end {
                                    if e + 4 < e_end {
                                        let next_v = (*edge_ptr.add(e + 4)).to as usize;
                                        _mm_prefetch(d_ptr.add(next_v) as *const i8, _MM_HINT_T0);
                                    }
                                    let edge = *edge_ptr.add(e);
                                    let v = edge.to as usize;
                                    let nd = d + edge.w as u64;
                                    let cur_vd = *d_ptr.add(v);
                                    if nd < cur_vd && nd < bound {
                                        if cur_vd == u64::MAX && !dirty_overflow {
                                            if dirty.len() < dirty_limit {
                                                dirty.push(v as u32);
                                            } else {
                                                dirty_overflow = true;
                                            }
                                        }
                                        *d_ptr.add(v) = nd;
                                        if remaining == 1 && v == curr_single_t as usize {
                                            bound = nd;
                                        } else if !use_sink_filter || (((*out_bit_ptr.add(v >> 6) | is_target[v >> 6]) & (1u64 << (v & 63))) != 0) {
                                            heap.push((nd << V_BITS) | (v as u64));
                                        }
                                    }
                                }
                            }
                        }

                        unsafe {
                            for k in grp.begin_idx..grp.end_idx {
                                let tk = active_ref[k].t as usize;
                                if remaining > 1 {
                                    is_target[tk >> 6] = 0;
                                }
                                let d = *d_ptr.add(tk);
                                let ans_val = if d == u64::MAX { -1 } else { d as i64 };
                                *answers_ptr.add(active_ref[k].id as usize) = ans_val;
                            }
                        }
                    }

                    if dirty_overflow {
                        dist.fill(u64::MAX);
                    } else {
                        unsafe {
                            for &u in &dirty {
                                *d_ptr.add(u as usize) = u64::MAX;
                            }
                        }
                    }
                }
            });
        }
    });

    let out_path = format!("{}/distances.txt", out_dir);
    let mut f = File::create(out_path).expect("Failed to create distances.txt");
    let mut out_buf = Vec::with_capacity(answers.len() * 12);
    let mut tmp = [0u8; 24];

    for ans in answers {
        if ans < 0 {
            out_buf.extend_from_slice(b"-1\n");
        } else if ans == 0 {
            out_buf.extend_from_slice(b"0\n");
        } else {
            let mut val = ans as u64;
            let mut idx = 23;
            tmp[idx] = b'\n';
            while val > 0 {
                idx -= 1;
                tmp[idx] = b'0' + (val % 10) as u8;
                val /= 10;
            }
            out_buf.extend_from_slice(&tmp[idx..24]);
        }
        if out_buf.len() >= 65536 {
            f.write_all(&out_buf).expect("Failed to write buffer");
            out_buf.clear();
        }
    }
    if !out_buf.is_empty() {
        f.write_all(&out_buf).expect("Failed to write buffer");
    }
}
