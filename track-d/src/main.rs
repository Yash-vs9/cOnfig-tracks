//! Speed Demon 2026 - Track D (Traverse)
//! Ultra-Fast Bit-Packed Dual-CSR Bidirectional Dijkstra Engine.

use std::fs::File;
use std::io::Write;
use std::os::unix::io::IntoRawFd;
use std::ptr;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

extern "C" {
    fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
    fn munmap(a: *mut u8, l: usize) -> i32;
    fn madvise(a: *mut u8, l: usize, adv: i32) -> i32;
    fn close(fd: i32) -> i32;
}

const W_BITS: u32 = 11;
const W_SENTINEL: u32 = (1 << W_BITS) - 1; // 2047
const V_BITS: u32 = 21;
const V_MASK: u32 = (1 << V_BITS) - 1;

#[derive(Clone, Copy)]
struct Query {
    s: u32,
    t: u32,
    id: u32,
}

#[derive(Clone, Copy)]
#[repr(C)]
struct DistPair {
    fwd: u64,
    rev: u64,
}

const HEAP_V_BITS: u32 = 22;
const HEAP_V_MASK: u64 = (1u64 << HEAP_V_BITS) - 1;

struct MinHeap4 {
    data: Vec<u64>,
}

impl MinHeap4 {
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
    fn min_key(&self) -> u64 {
        if self.data.is_empty() {
            u64::MAX
        } else {
            self.data[0] >> HEAP_V_BITS
        }
    }
    #[inline(always)]
    fn push(&mut self, packed: u64) {
        let mut i = self.data.len();
        self.data.push(packed);
        unsafe {
            let slice = self.data.as_mut_ptr();
            while i > 0 {
                let p = (i - 1) >> 2;
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
    fn pop(&mut self) -> (u64, u32) {
        unsafe {
            let top = *self.data.get_unchecked(0);
            let last = self.data.pop().unwrap_unchecked();
            let sz = self.data.len();
            if sz > 0 {
                let slice = self.data.as_mut_ptr();
                let mut i = 0;
                loop {
                    let c = (i << 2) + 1;
                    if c >= sz {
                        break;
                    }
                    let mut mc = c;
                    let ec = (c + 4).min(sz);
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
            (top >> HEAP_V_BITS, (top & HEAP_V_MASK) as u32)
        }
    }
}

#[inline(always)]
unsafe fn parse_fast_u32(p: &mut *const u8) -> u32 {
    let mut v = (**p - b'0') as u32;
    *p = p.add(1);
    while **p >= b'0' {
        v = v * 10 + (**p - b'0') as u32;
        *p = p.add(1);
    }
    v
}

fn main() {
    let (data_dir, out_dir) = {
        let args: Vec<String> = std::env::args().collect();
        let mut d = "/data".to_string();
        let mut o = "/out".to_string();
        let mut i = 1;
        let mut pos = 0;
        while i < args.len() {
            if args[i] == "--data" && i + 1 < args.len() {
                d = args[i + 1].clone();
                i += 2;
            } else if args[i] == "--out" && i + 1 < args.len() {
                o = args[i + 1].clone();
                i += 2;
            } else if !args[i].starts_with('-') {
                if pos == 0 {
                    d = args[i].clone();
                } else {
                    o = args[i].clone();
                }
                pos += 1;
                i += 1;
            } else {
                i += 1;
            }
        }
        (d, o)
    };

    let gf = File::open(format!("{}/graph.txt", data_dir)).expect("graph.txt");
    let gsize = gf.metadata().expect("meta").len() as usize;
    let gfd = gf.into_raw_fd();
    let g_ptr = unsafe { mmap(ptr::null_mut(), gsize, 1, 2, gfd, 0) };
    unsafe {
        close(gfd);
        madvise(g_ptr, gsize, 2);
    }
    let g_bytes = unsafe { std::slice::from_raw_parts(g_ptr, gsize) };

    let mut g_end = g_bytes.len();
    while g_end > 0 && g_bytes[g_end - 1] < b'0' {
        g_end -= 1;
    }
    let mut p = g_bytes.as_ptr();
    let end_p = unsafe { p.add(g_end) };

    let n = unsafe { parse_fast_u32(&mut p) };
    unsafe {
        p = p.add(1);
    }
    let _m = unsafe { parse_fast_u32(&mut p) };
    unsafe {
        p = p.add(1);
    }

    let edge_start_addr = p as usize;
    let nt = std::thread::available_parallelism()
        .map(|v| v.get())
        .unwrap_or(12)
        .min(12);

    let edge_bytes = (end_p as usize) - edge_start_addr;
    let mut chunk_bounds = vec![end_p as usize; nt + 1];
    chunk_bounds[0] = edge_start_addr;
    for t in 1..nt {
        let mut cp = (edge_start_addr + (edge_bytes * t) / nt) as *const u8;
        unsafe {
            while cp < end_p && *(cp.sub(1)) != b'\n' {
                cp = cp.add(1);
            }
        }
        chunk_bounds[t] = cp as usize;
    }

    let head_fwd: Vec<AtomicU32> = (0..=n).map(|_| AtomicU32::new(0)).collect();
    let head_rev: Vec<AtomicU32> = (0..=n).map(|_| AtomicU32::new(0)).collect();

    // Pass 1: Parallel degree counting
    std::thread::scope(|scope| {
        for t in 0..nt {
            let start = chunk_bounds[t];
            let finish = chunk_bounds[t + 1];
            let hf = &head_fwd;
            let hr = &head_rev;

            scope.spawn(move || {
                let mut cur = start as *const u8;
                let end_cur = finish as *const u8;
                unsafe {
                    while cur < end_cur {
                        let u = parse_fast_u32(&mut cur);
                        cur = cur.add(1);
                        let v = parse_fast_u32(&mut cur);
                        cur = cur.add(1);
                        let _w = parse_fast_u32(&mut cur);
                        if *cur == b'\r' {
                            cur = cur.add(1);
                        }
                        cur = cur.add(1);
                        if u != v {
                            hf.get_unchecked(u as usize).fetch_add(1, Ordering::Relaxed);
                            hr.get_unchecked(v as usize).fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            });
        }
    });

    let mut accum_fwd = 0u32;
    let head_fwd_pos: Vec<AtomicU32> = head_fwd
        .into_iter()
        .map(|a| {
            let d = a.load(Ordering::Relaxed);
            let pos = accum_fwd;
            accum_fwd += d;
            AtomicU32::new(pos)
        })
        .collect();

    let mut accum_rev = 0u32;
    let head_rev_pos: Vec<AtomicU32> = head_rev
        .into_iter()
        .map(|a| {
            let d = a.load(Ordering::Relaxed);
            let pos = accum_rev;
            accum_rev += d;
            AtomicU32::new(pos)
        })
        .collect();

    let mut edges_fwd = vec![0u32; accum_fwd as usize];
    let mut edges_rev = vec![0u32; accum_rev as usize];
    let edges_fwd_addr = edges_fwd.as_mut_ptr() as usize;
    let edges_rev_addr = edges_rev.as_mut_ptr() as usize;

    // Pass 2: Populate both CSRs with 32-bit packed edges
    let overflows: Vec<Vec<(u32, u32)>> = std::thread::scope(|scope| {
        let mut handlers = Vec::with_capacity(nt);
        for t in 0..nt {
            let start = chunk_bounds[t];
            let finish = chunk_bounds[t + 1];
            let hf_pos = &head_fwd_pos;
            let hr_pos = &head_rev_pos;

            handlers.push(scope.spawn(move || {
                let mut local_overflow = Vec::new();
                let mut cur = start as *const u8;
                let end_cur = finish as *const u8;
                let ef_ptr = edges_fwd_addr as *mut u32;
                let er_ptr = edges_rev_addr as *mut u32;

                unsafe {
                    while cur < end_cur {
                        let u = parse_fast_u32(&mut cur);
                        cur = cur.add(1);
                        let v = parse_fast_u32(&mut cur);
                        cur = cur.add(1);
                        let w = parse_fast_u32(&mut cur);
                        if *cur == b'\r' {
                            cur = cur.add(1);
                        }
                        cur = cur.add(1);

                        if u != v {
                            let pw = if w < W_SENTINEL { w } else { W_SENTINEL };
                            let idx_f = hf_pos
                                .get_unchecked(u as usize)
                                .fetch_add(1, Ordering::Relaxed)
                                as usize;
                            *ef_ptr.add(idx_f) = (pw << V_BITS) | (v & V_MASK);

                            let idx_r = hr_pos
                                .get_unchecked(v as usize)
                                .fetch_add(1, Ordering::Relaxed)
                                as usize;
                            *er_ptr.add(idx_r) = (pw << V_BITS) | (u & V_MASK);

                            if pw == W_SENTINEL {
                                local_overflow.push((idx_f as u32, w));
                                local_overflow.push(((1u32 << 31) | (idx_r as u32), w));
                            }
                        }
                    }
                }
                local_overflow
            }));
        }
        handlers.into_iter().map(|h| h.join().unwrap()).collect()
    });

    unsafe {
        munmap(g_ptr, gsize);
    }
    drop(chunk_bounds);

    let mut overflow_map: Vec<(u32, u32)> = overflows.into_iter().flatten().collect();
    overflow_map.sort_unstable_by_key(|&(idx, _)| idx);

    let mut head_fwd: Vec<u32> = head_fwd_pos.into_iter().map(|a| a.into_inner()).collect();
    let mut head_rev: Vec<u32> = head_rev_pos.into_iter().map(|a| a.into_inner()).collect();
    if n > 0 {
        head_fwd.copy_within(0..n as usize, 1);
        head_rev.copy_within(0..n as usize, 1);
    }
    head_fwd[0] = 0;
    head_rev[0] = 0;

    // Parse queries
    let qf = File::open(format!("{}/queries.txt", data_dir)).expect("queries.txt");
    let qsize = qf.metadata().expect("qmeta").len() as usize;
    let qfd = qf.into_raw_fd();
    let q_ptr = unsafe { mmap(ptr::null_mut(), qsize, 1, 2, qfd, 0) };
    unsafe {
        close(qfd);
    }
    let q_bytes = unsafe { std::slice::from_raw_parts(q_ptr, qsize) };
    let mut queries = Vec::with_capacity(4096);
    let mut qp = q_bytes.as_ptr();
    let q_end_p = unsafe { qp.add(q_bytes.len()) };
    let mut qid = 0u32;
    unsafe {
        while qp < q_end_p {
            while qp < q_end_p && *qp < b'0' {
                qp = qp.add(1);
            }
            if qp >= q_end_p {
                break;
            }
            let s = parse_fast_u32(&mut qp);
            while qp < q_end_p && *qp < b'0' {
                qp = qp.add(1);
            }
            let t = parse_fast_u32(&mut qp);
            queries.push(Query { s, t, id: qid });
            qid += 1;
        }
        munmap(q_ptr, qsize);
    }

    let mut answers = vec![-1i64; queries.len()];
    let answers_addr = answers.as_mut_ptr() as usize;

    let q_threads = nt.min(4).min(queries.len());
    let next_query = AtomicUsize::new(0);

    let ef_addr = edges_fwd.as_ptr() as usize;
    let er_addr = edges_rev.as_ptr() as usize;
    let hf_addr = head_fwd.as_ptr() as usize;
    let hr_addr = head_rev.as_ptr() as usize;
    let oflow_ref = &overflow_map;

    std::thread::scope(|scope| {
        for _ in 0..q_threads {
            let next_ref = &next_query;
            let q_ref = &queries;

            scope.spawn(move || {
                let mut dist = vec![
                    DistPair {
                        fwd: u64::MAX,
                        rev: u64::MAX,
                    };
                    n as usize
                ];
                let mut dirty = Vec::with_capacity(16384);
                let mut heap_f = MinHeap4::with_capacity(16384);
                let mut heap_b = MinHeap4::with_capacity(16384);

                let d_ptr = dist.as_mut_ptr();
                let ef_ptr = ef_addr as *const u32;
                let er_ptr = er_addr as *const u32;
                let hf_ptr = hf_addr as *const u32;
                let hr_ptr = hr_addr as *const u32;
                let ans_ptr = answers_addr as *mut i64;

                loop {
                    let qi = next_ref.fetch_add(1, Ordering::Relaxed);
                    if qi >= q_ref.len() {
                        break;
                    }

                    let q = q_ref[qi];
                    let s = q.s as usize;
                    let t = q.t as usize;
                    if s == t {
                        unsafe {
                            *ans_ptr.add(q.id as usize) = 0;
                        }
                        continue;
                    }

                    unsafe {
                        let sf_deg = *hf_ptr.add(s + 1) - *hf_ptr.add(s);
                        let tr_deg = *hr_ptr.add(t + 1) - *hr_ptr.add(t);
                        if sf_deg == 0 || tr_deg == 0 {
                            *ans_ptr.add(q.id as usize) = -1;
                            continue;
                        }
                    }

                    heap_f.clear();
                    heap_b.clear();
                    dirty.clear();

                    unsafe {
                        (*d_ptr.add(s)).fwd = 0;
                        (*d_ptr.add(t)).rev = 0;
                    }
                    dirty.push(s as u32);
                    dirty.push(t as u32);
                    heap_f.push(s as u64);
                    heap_b.push(t as u64);

                    let mut mu = u64::MAX;

                    unsafe {
                        while !heap_f.is_empty() && !heap_b.is_empty() {
                            if mu != u64::MAX {
                                let top_f = heap_f.min_key();
                                let top_b = heap_b.min_key();
                                if top_f.saturating_add(top_b) >= mu {
                                    break;
                                }
                            }

                            let is_fwd = heap_f.data.len() <= heap_b.data.len();
                            if is_fwd {
                                let (d, u) = heap_f.pop();
                                let u = u as usize;
                                let pair = d_ptr.add(u);
                                if (*pair).fwd == u64::MAX || d > (*pair).fwd {
                                    continue;
                                }
                                let e_start = *hf_ptr.add(u) as usize;
                                let e_end = *hf_ptr.add(u + 1) as usize;
                                for e in e_start..e_end {
                                    let packed_edge = *ef_ptr.add(e);
                                    let v = (packed_edge & V_MASK) as usize;
                                    let mut w = (packed_edge >> V_BITS) as u64;
                                    if w == W_SENTINEL as u64 {
                                        let key = e as u32;
                                        if let Ok(pos) =
                                            oflow_ref.binary_search_by_key(&key, |&(k, _)| k)
                                        {
                                            w = oflow_ref[pos].1 as u64;
                                        }
                                    }
                                    let nd = d + w;
                                    if nd >= mu {
                                        continue;
                                    }
                                    let v_pair = d_ptr.add(v);
                                    let cur_fwd = (*v_pair).fwd;
                                    if nd < cur_fwd {
                                        let cur_rev = (*v_pair).rev;
                                        if cur_fwd == u64::MAX && cur_rev == u64::MAX {
                                            dirty.push(v as u32);
                                        }
                                        (*v_pair).fwd = nd;
                                        heap_f.push((nd << HEAP_V_BITS) | (v as u64));
                                        if cur_rev != u64::MAX {
                                            let cand = nd + cur_rev;
                                            if cand < mu {
                                                mu = cand;
                                            }
                                        }
                                    }
                                }
                            } else {
                                let (d, u) = heap_b.pop();
                                let u = u as usize;
                                let pair = d_ptr.add(u);
                                if (*pair).rev == u64::MAX || d > (*pair).rev {
                                    continue;
                                }
                                let e_start = *hr_ptr.add(u) as usize;
                                let e_end = *hr_ptr.add(u + 1) as usize;
                                for e in e_start..e_end {
                                    let packed_edge = *er_ptr.add(e);
                                    let v = (packed_edge & V_MASK) as usize;
                                    let mut w = (packed_edge >> V_BITS) as u64;
                                    if w == W_SENTINEL as u64 {
                                        let key = (1u32 << 31) | (e as u32);
                                        if let Ok(pos) =
                                            oflow_ref.binary_search_by_key(&key, |&(k, _)| k)
                                        {
                                            w = oflow_ref[pos].1 as u64;
                                        }
                                    }
                                    let nd = d + w;
                                    if nd >= mu {
                                        continue;
                                    }
                                    let v_pair = d_ptr.add(v);
                                    let cur_rev = (*v_pair).rev;
                                    if nd < cur_rev {
                                        let cur_fwd = (*v_pair).fwd;
                                        if cur_rev == u64::MAX && cur_fwd == u64::MAX {
                                            dirty.push(v as u32);
                                        }
                                        (*v_pair).rev = nd;
                                        heap_b.push((nd << HEAP_V_BITS) | (v as u64));
                                        if cur_fwd != u64::MAX {
                                            let cand = nd + cur_fwd;
                                            if cand < mu {
                                                mu = cand;
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        let ans_val = if mu == u64::MAX { -1 } else { mu as i64 };
                        *ans_ptr.add(q.id as usize) = ans_val;

                        for &v in &dirty {
                            *d_ptr.add(v as usize) = DistPair {
                                fwd: u64::MAX,
                                rev: u64::MAX,
                            };
                        }
                    }
                }
            });
        }
    });

    let out_path = format!("{}/distances.txt", out_dir);
    let mut f = File::create(out_path).expect("distances.txt");
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
    }
    f.write_all(&out_buf).expect("write");
}
