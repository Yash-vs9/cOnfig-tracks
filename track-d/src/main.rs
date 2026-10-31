//! Speed Demon 2026 - Track D (Traverse)
//! Ultra-Low-RSS, 12-Thread Cache-Prefetched Bidirectional Dijkstra Engine.
use std::fs::File;
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::{_mm_prefetch, _MM_HINT_T0};

const W_BITS: u32 = 11;
const W_SENTINEL: u32 = (1 << W_BITS) - 1; // 2047
const V_BITS: u32 = 21;
const V_MASK: u32 = (1 << V_BITS) - 1;
const INF32: u32 = u32::MAX;

#[derive(Clone, Copy)]
struct Query {
    s: u32,
    t: u32,
    id: u32,
}

#[derive(Clone, Copy)]
#[repr(C, align(8))]
struct DistPair {
    fwd: u32,
    rev: u32,
}

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
    fn min_key(&self) -> u32 {
        if self.data.is_empty() {
            INF32
        } else {
            unsafe { (*self.data.get_unchecked(0) >> 32) as u32 }
        }
    }
    #[inline(always)]
    fn push(&mut self, d: u32, v: u32) {
        let packed = ((d as u64) << 32) | (v as u64);
        let mut i = self.data.len();
        self.data.push(packed);
        unsafe {
            let slice = self.data.as_mut_ptr();
            while i > 0 {
                let p = (i - 1) >> 2;
                let parent = *slice.add(p);
                if packed >= parent {
                    break;
                }
                *slice.add(i) = parent;
                i = p;
            }
            *slice.add(i) = packed;
        }
    }
    #[inline(always)]
    fn pop(&mut self) -> (u32, u32) {
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
            ((top >> 32) as u32, top as u32)
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

const STREAM_BUF_SIZE: usize = 256 * 1024;

fn solve_query_64(
    s: usize,
    t: usize,
    n: usize,
    hf_ptr: *const u32,
    hr_ptr: *const u32,
    ef_ptr: *const u32,
    er_ptr: *const u32,
    oflow: &[(u32, u32)],
) -> i64 {
    #[derive(Clone, Copy)]
    struct Dist64 {
        fwd: u64,
        rev: u64,
    }
    let mut dist = vec![
        Dist64 {
            fwd: u64::MAX,
            rev: u64::MAX
        };
        n
    ];
    let mut dirty = Vec::with_capacity(16384);
    let mut heap_f = Vec::new();
    let mut heap_b = Vec::new();

    dist[s].fwd = 0;
    dist[t].rev = 0;
    dirty.push(s);
    dirty.push(t);
    heap_f.push((0u64, s as u32));
    heap_b.push((0u64, t as u32));

    let mut mu = u64::MAX;
    unsafe {
        while !heap_f.is_empty() && !heap_b.is_empty() {
            let top_f = heap_f.iter().map(|&(d, _)| d).min().unwrap_or(u64::MAX);
            let top_b = heap_b.iter().map(|&(d, _)| d).min().unwrap_or(u64::MAX);
            if top_f.saturating_add(top_b) >= mu {
                break;
            }

            let is_fwd = heap_f.len() <= heap_b.len();
            let (d, u) = if is_fwd {
                let min_idx = heap_f
                    .iter()
                    .enumerate()
                    .min_by_key(|&(_, &(d, _))| d)
                    .map(|(i, _)| i)
                    .unwrap();
                heap_f.swap_remove(min_idx)
            } else {
                let min_idx = heap_b
                    .iter()
                    .enumerate()
                    .min_by_key(|&(_, &(d, _))| d)
                    .map(|(i, _)| i)
                    .unwrap();
                heap_b.swap_remove(min_idx)
            };
            let u = u as usize;
            let cur_dist = if is_fwd { dist[u].fwd } else { dist[u].rev };
            if d > cur_dist {
                continue;
            }

            let (h_ptr, e_ptr) = if is_fwd {
                (hf_ptr, ef_ptr)
            } else {
                (hr_ptr, er_ptr)
            };
            let e_start = *h_ptr.add(u) as usize;
            let e_end = *h_ptr.add(u + 1) as usize;
            for e in e_start..e_end {
                let packed_edge = *e_ptr.add(e);
                let v = (packed_edge & V_MASK) as usize;
                let mut w = (packed_edge >> V_BITS) as u64;
                if w == W_SENTINEL as u64 {
                    let key = if is_fwd {
                        e as u32
                    } else {
                        (1u32 << 31) | (e as u32)
                    };
                    if let Ok(pos) = oflow.binary_search_by_key(&key, |&(k, _)| k) {
                        w = oflow[pos].1 as u64;
                    }
                }
                let nd = d.saturating_add(w);
                if nd >= mu {
                    continue;
                }
                let cur_self = if is_fwd { dist[v].fwd } else { dist[v].rev };
                let cur_other = if is_fwd { dist[v].rev } else { dist[v].fwd };
                if nd < cur_self {
                    if cur_self == u64::MAX && cur_other == u64::MAX {
                        dirty.push(v);
                    }
                    if is_fwd {
                        dist[v].fwd = nd;
                        heap_f.push((nd, v as u32));
                    } else {
                        dist[v].rev = nd;
                        heap_b.push((nd, v as u32));
                    }
                    if cur_other != u64::MAX {
                        let cand = nd.saturating_add(cur_other);
                        if cand < mu {
                            mu = cand;
                        }
                    }
                }
            }
        }
    }
    if mu == u64::MAX {
        -1
    } else {
        mu as i64
    }
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

    let mut header_buf = [0u8; 64];
    let h_read = gf.read_at(&mut header_buf, 0).expect("header read");
    let mut p = header_buf.as_ptr();
    let n = unsafe { parse_fast_u32(&mut p) };
    unsafe {
        p = p.add(1);
    }
    let _m = unsafe { parse_fast_u32(&mut p) };
    unsafe {
        if *p == b'\r' {
            p = p.add(1);
        }
        p = p.add(1);
    }
    let edge_start_offset = (p as usize) - (header_buf.as_ptr() as usize);
    let _ = h_read;

    let mut tail_buf = [0u8; 64];
    let tail_start = gsize.saturating_sub(64);
    let t_read = gf.read_at(&mut tail_buf, tail_start as u64).unwrap_or(0);
    let mut g_end = tail_start + t_read;
    while g_end > tail_start && tail_buf[g_end - tail_start - 1] < b'0' {
        g_end -= 1;
    }

    let nt = std::thread::available_parallelism()
        .map(|v| v.get())
        .unwrap_or(12)
        .min(12);

    let edge_bytes = g_end - edge_start_offset;
    let mut chunk_bounds = vec![g_end; nt + 1];
    chunk_bounds[0] = edge_start_offset;
    for t in 1..nt {
        let mut off = edge_start_offset + (edge_bytes * t) / nt;
        let mut probe = [0u8; 128];
        let pr = gf
            .read_at(&mut probe, off.saturating_sub(1) as u64)
            .unwrap_or(0);
        let mut shift = 0;
        while shift + 1 < pr && off < g_end && probe[shift] != b'\n' {
            off += 1;
            shift += 1;
        }
        chunk_bounds[t] = off.min(g_end);
    }

    let head_fwd: Vec<AtomicU32> = unsafe {
        let v = vec![0u32; (n + 1) as usize];
        std::mem::transmute(v)
    };
    let head_rev: Vec<AtomicU32> = unsafe {
        let v = vec![0u32; (n + 1) as usize];
        std::mem::transmute(v)
    };

    std::thread::scope(|scope| {
        for t in 0..nt {
            let start = chunk_bounds[t];
            let finish = chunk_bounds[t + 1];
            let hf = &head_fwd;
            let hr = &head_rev;
            let gf_ref = &gf;

            scope.spawn(move || {
                let mut buf = vec![0u8; STREAM_BUF_SIZE + 64];
                let mut file_off = start;
                let mut leftover = 0usize;

                while file_off < finish {
                    let to_read = (finish - file_off).min(STREAM_BUF_SIZE);
                    let got = gf_ref
                        .read_at(&mut buf[leftover..leftover + to_read], file_off as u64)
                        .unwrap_or(0);
                    if got == 0 {
                        break;
                    }
                    file_off += got;
                    let total = leftover + got;

                    let mut valid_end = total;
                    if file_off < finish {
                        while valid_end > 0 && buf[valid_end - 1] != b'\n' {
                            valid_end -= 1;
                        }
                    } else if valid_end == total {
                        buf[total] = b'\n';
                    }

                    unsafe {
                        let mut cur = buf.as_ptr();
                        let end_cur = cur.add(valid_end);
                        while cur < end_cur {
                            let u = parse_fast_u32(&mut cur);
                            cur = cur.add(1);
                            let v = parse_fast_u32(&mut cur);
                            while *cur != b'\n' {
                                cur = cur.add(1);
                            }
                            cur = cur.add(1);
                            if u != v {
                                hf.get_unchecked(u as usize).fetch_add(1, Ordering::Relaxed);
                                hr.get_unchecked(v as usize).fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }

                    leftover = total - valid_end;
                    if leftover > 0 {
                        buf.copy_within(valid_end..total, 0);
                    }
                }
            });
        }
    });

    let mut accum_fwd = 0u32;
    for a in &head_fwd {
        let d = a.load(Ordering::Relaxed);
        a.store(accum_fwd, Ordering::Relaxed);
        accum_fwd += d;
    }

    let mut accum_rev = 0u32;
    for a in &head_rev {
        let d = a.load(Ordering::Relaxed);
        a.store(accum_rev, Ordering::Relaxed);
        accum_rev += d;
    }

    let mut edges_fwd = vec![0u32; accum_fwd as usize];
    let mut edges_rev = vec![0u32; accum_rev as usize];
    let edges_fwd_addr = edges_fwd.as_mut_ptr() as usize;
    let edges_rev_addr = edges_rev.as_mut_ptr() as usize;

    let overflows: Vec<Vec<(u32, u32)>> = std::thread::scope(|scope| {
        let mut handlers = Vec::with_capacity(nt);
        for t in 0..nt {
            let start = chunk_bounds[t];
            let finish = chunk_bounds[t + 1];
            let hf_pos = &head_fwd;
            let hr_pos = &head_rev;
            let gf_ref = &gf;

            handlers.push(scope.spawn(move || {
                let mut local_overflow = Vec::new();
                let mut buf = vec![0u8; STREAM_BUF_SIZE + 64];
                let mut file_off = start;
                let mut leftover = 0usize;
                let ef_ptr = edges_fwd_addr as *mut u32;
                let er_ptr = edges_rev_addr as *mut u32;

                while file_off < finish {
                    let to_read = (finish - file_off).min(STREAM_BUF_SIZE);
                    let got = gf_ref
                        .read_at(&mut buf[leftover..leftover + to_read], file_off as u64)
                        .unwrap_or(0);
                    if got == 0 {
                        break;
                    }
                    file_off += got;
                    let total = leftover + got;

                    let mut valid_end = total;
                    if file_off < finish {
                        while valid_end > 0 && buf[valid_end - 1] != b'\n' {
                            valid_end -= 1;
                        }
                    } else if valid_end == total {
                        buf[total] = b'\n';
                    }

                    unsafe {
                        let mut cur = buf.as_ptr();
                        let end_cur = cur.add(valid_end);
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

                    leftover = total - valid_end;
                    if leftover > 0 {
                        buf.copy_within(valid_end..total, 0);
                    }
                }
                local_overflow
            }));
        }
        handlers.into_iter().map(|h| h.join().unwrap()).collect()
    });

    drop(gf);
    drop(chunk_bounds);

    let mut overflow_map: Vec<(u32, u32)> = overflows.into_iter().flatten().collect();
    overflow_map.sort_unstable_by_key(|&(idx, _)| idx);

    let mut head_fwd: Vec<u32> = unsafe { std::mem::transmute(head_fwd) };
    let mut head_rev: Vec<u32> = unsafe { std::mem::transmute(head_rev) };
    if n > 0 {
        head_fwd.copy_within(0..n as usize, 1);
        head_rev.copy_within(0..n as usize, 1);
    }
    head_fwd[0] = 0;
    head_rev[0] = 0;

    let q_bytes = std::fs::read(format!("{}/queries.txt", data_dir)).expect("queries.txt");
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
    }
    drop(q_bytes);

    let mut answers = vec![-1i64; queries.len()];
    let answers_addr = answers.as_mut_ptr() as usize;

    let q_threads = nt.min(queries.len());
    let next_query = AtomicUsize::new(0);

    let ef_addr = edges_fwd.as_ptr() as usize;
    let er_addr = edges_rev.as_ptr() as usize;
    let hf_addr = head_fwd.as_ptr() as usize;
    let hr_addr = head_rev.as_ptr() as usize;
    let oflow_ref = &overflow_map;
    let has_overflow = !overflow_map.is_empty();

    std::thread::scope(|scope| {
        for _ in 0..q_threads {
            let next_ref = &next_query;
            let q_ref = &queries;

            scope.spawn(move || {
                let mut dist = vec![
                    DistPair {
                        fwd: INF32,
                        rev: INF32,
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
                    heap_f.push(0, s as u32);
                    heap_b.push(0, t as u32);

                    let mut mu = INF32;

                    unsafe {
                        while !heap_f.is_empty() && !heap_b.is_empty() {
                            let top_f = heap_f.min_key();
                            let top_b = heap_b.min_key();
                            if top_f.saturating_add(top_b) >= mu {
                                break;
                            }

                            let is_fwd = heap_f.data.len() <= heap_b.data.len();
                            if is_fwd {
                                let (d, u) = heap_f.pop();
                                let u = u as usize;
                                let pair = d_ptr.add(u);
                                if d > (*pair).fwd {
                                    continue;
                                }
                                let e_start = *hf_ptr.add(u) as usize;
                                let e_end = *hf_ptr.add(u + 1) as usize;
                                for e in e_start..e_end {
                                    #[cfg(target_arch = "x86_64")]
                                    if e + 2 < e_end {
                                        let next_v = (*ef_ptr.add(e + 2) & V_MASK) as usize;
                                        _mm_prefetch(d_ptr.add(next_v) as *const i8, _MM_HINT_T0);
                                    }

                                    let packed_edge = *ef_ptr.add(e);
                                    let v = (packed_edge & V_MASK) as usize;
                                    let mut w = packed_edge >> V_BITS;
                                    if has_overflow && w == W_SENTINEL {
                                        let key = e as u32;
                                        if let Ok(pos) =
                                            oflow_ref.binary_search_by_key(&key, |&(k, _)| k)
                                        {
                                            w = oflow_ref.get_unchecked(pos).1;
                                        }
                                    }
                                    let nd = d.saturating_add(w);
                                    if nd >= mu {
                                        continue;
                                    }
                                    let v_pair = d_ptr.add(v);
                                    let cur_fwd = (*v_pair).fwd;
                                    if nd < cur_fwd {
                                        let cur_rev = (*v_pair).rev;
                                        if cur_fwd == INF32 && cur_rev == INF32 {
                                            dirty.push(v as u32);
                                        }
                                        (*v_pair).fwd = nd;
                                        heap_f.push(nd, v as u32);
                                        if cur_rev != INF32 {
                                            let cand = nd.saturating_add(cur_rev);
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
                                if d > (*pair).rev {
                                    continue;
                                }
                                let e_start = *hr_ptr.add(u) as usize;
                                let e_end = *hr_ptr.add(u + 1) as usize;
                                for e in e_start..e_end {
                                    #[cfg(target_arch = "x86_64")]
                                    if e + 2 < e_end {
                                        let next_v = (*er_ptr.add(e + 2) & V_MASK) as usize;
                                        _mm_prefetch(d_ptr.add(next_v) as *const i8, _MM_HINT_T0);
                                    }

                                    let packed_edge = *er_ptr.add(e);
                                    let v = (packed_edge & V_MASK) as usize;
                                    let mut w = packed_edge >> V_BITS;
                                    if has_overflow && w == W_SENTINEL {
                                        let key = (1u32 << 31) | (e as u32);
                                        if let Ok(pos) =
                                            oflow_ref.binary_search_by_key(&key, |&(k, _)| k)
                                        {
                                            w = oflow_ref.get_unchecked(pos).1;
                                        }
                                    }
                                    let nd = d.saturating_add(w);
                                    if nd >= mu {
                                        continue;
                                    }
                                    let v_pair = d_ptr.add(v);
                                    let cur_rev = (*v_pair).rev;
                                    if nd < cur_rev {
                                        let cur_fwd = (*v_pair).fwd;
                                        if cur_rev == INF32 && cur_fwd == INF32 {
                                            dirty.push(v as u32);
                                        }
                                        (*v_pair).rev = nd;
                                        heap_b.push(nd, v as u32);
                                        if cur_fwd != INF32 {
                                            let cand = nd.saturating_add(cur_fwd);
                                            if cand < mu {
                                                mu = cand;
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        let mut ans_val = if mu == INF32 { -1 } else { mu as i64 };
                        if ans_val == -1 {
                            // Fallback for giant-weight paths (> 2^31)
                            ans_val = solve_query_64(
                                s, t, n as usize, hf_ptr, hr_ptr, ef_ptr, er_ptr, oflow_ref,
                            );
                        }
                        *ans_ptr.add(q.id as usize) = ans_val;

                        for &v in &dirty {
                            *d_ptr.add(v as usize) = DistPair {
                                fwd: INF32,
                                rev: INF32,
                            };
                        }
                    }
                }
            });
        }
    });

    let _ = std::fs::create_dir_all(&out_dir);
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
