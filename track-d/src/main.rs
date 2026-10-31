//! Speed Demon 2026 - Track D (Traverse)
use std::fs::File;
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

const W_BITS: u32 = 11;
const W_SENTINEL: u32 = (1 << W_BITS) - 1;
const V_BITS: u32 = 21;
const V_MASK: u32 = (1 << V_BITS) - 1;
const INF32: u32 = u32::MAX;
const H_SZ: usize = 65536;
const H_MSK: usize = H_SZ - 1;
const H_MAX_LOAD: u32 = 55705; // 85% of 65536
const STREAM_BUF_SIZE: usize = 256 * 1024;

#[derive(Clone, Copy)]
struct Query { s: u32, t: u32, id: u32 }

#[derive(Clone, Copy)]
#[repr(C, align(16))]
struct Slot { v: u32, fwd: u32, rev: u32, epoch: u32 }

struct FastDistMap { slots: Vec<Slot>, epoch: u32, used: u32 }
impl FastDistMap {
    fn new() -> Self {
        Self { slots: unsafe { std::mem::transmute(vec![0u128; H_SZ]) }, epoch: 1, used: 0 }
    }
    #[inline(always)]
    fn next_query(&mut self) {
        self.used = 0;
        self.epoch = self.epoch.wrapping_add(1);
        if self.epoch == 0 { for s in &mut self.slots { s.epoch = 0; } self.epoch = 1; }
    }
    #[inline(always)]
    unsafe fn init_start(&mut self, s: u32, t: u32) {
        let ep = self.epoch;
        let mut idx = (s.wrapping_mul(0x9E3779B9) as usize) & H_MSK;
        let base = self.slots.as_mut_ptr();
        loop {
            let slot = base.add(idx);
            if (*slot).epoch != ep || (*slot).v == s {
                (*slot).epoch = ep; (*slot).v = s; (*slot).fwd = 0; (*slot).rev = INF32;
                self.used += 1;
                break;
            }
            idx = (idx + 1) & H_MSK;
        }
        let mut idx_t = (t.wrapping_mul(0x9E3779B9) as usize) & H_MSK;
        loop {
            let slot = base.add(idx_t);
            if (*slot).epoch != ep || (*slot).v == t {
                (*slot).epoch = ep; (*slot).v = t; (*slot).fwd = INF32; (*slot).rev = 0;
                self.used += 1;
                break;
            }
            idx_t = (idx_t + 1) & H_MSK;
        }
    }
    #[inline(always)]
    unsafe fn get_fwd_dist(&self, v: u32) -> u32 {
        let ep = self.epoch;
        let mut idx = (v.wrapping_mul(0x9E3779B9) as usize) & H_MSK;
        let base = self.slots.as_ptr();
        loop {
            let slot = base.add(idx);
            if (*slot).epoch != ep { return INF32; }
            if (*slot).v == v { return (*slot).fwd; }
            idx = (idx + 1) & H_MSK;
        }
    }
    #[inline(always)]
    unsafe fn get_rev_dist(&self, v: u32) -> u32 {
        let ep = self.epoch;
        let mut idx = (v.wrapping_mul(0x9E3779B9) as usize) & H_MSK;
        let base = self.slots.as_ptr();
        loop {
            let slot = base.add(idx);
            if (*slot).epoch != ep { return INF32; }
            if (*slot).v == v { return (*slot).rev; }
            idx = (idx + 1) & H_MSK;
        }
    }
    #[inline(always)]
    unsafe fn relax_fwd(&mut self, v: u32, nd: u32, heap: &mut MinHeap4, mu: &mut u32) {
        let ep = self.epoch;
        let mut idx = (v.wrapping_mul(0x9E3779B9) as usize) & H_MSK;
        let base = self.slots.as_mut_ptr();
        loop {
            let slot = base.add(idx);
            if (*slot).epoch != ep {
                (*slot).epoch = ep; (*slot).v = v; (*slot).fwd = nd; (*slot).rev = INF32;
                self.used += 1;
                heap.push(nd, v);
                return;
            }
            if (*slot).v == v {
                if nd < (*slot).fwd {
                    (*slot).fwd = nd;
                    heap.push(nd, v);
                    let cr = (*slot).rev;
                    if cr != INF32 { *mu = (*mu).min(nd.saturating_add(cr)); }
                }
                return;
            }
            idx = (idx + 1) & H_MSK;
        }
    }
    #[inline(always)]
    unsafe fn relax_rev(&mut self, v: u32, nd: u32, heap: &mut MinHeap4, mu: &mut u32) {
        let ep = self.epoch;
        let mut idx = (v.wrapping_mul(0x9E3779B9) as usize) & H_MSK;
        let base = self.slots.as_mut_ptr();
        loop {
            let slot = base.add(idx);
            if (*slot).epoch != ep {
                (*slot).epoch = ep; (*slot).v = v; (*slot).fwd = INF32; (*slot).rev = nd;
                self.used += 1;
                heap.push(nd, v);
                return;
            }
            if (*slot).v == v {
                if nd < (*slot).rev {
                    (*slot).rev = nd;
                    heap.push(nd, v);
                    let cf = (*slot).fwd;
                    if cf != INF32 { *mu = (*mu).min(nd.saturating_add(cf)); }
                }
                return;
            }
            idx = (idx + 1) & H_MSK;
        }
    }
}

struct MinHeap4 { data: Vec<u64> }
impl MinHeap4 {
    #[inline(always)] fn with_capacity(cap: usize) -> Self { Self { data: Vec::with_capacity(cap) } }
    #[inline(always)] fn clear(&mut self) { self.data.clear(); }
    #[inline(always)] fn is_empty(&self) -> bool { self.data.is_empty() }
    #[inline(always)] fn min_key(&self) -> u32 {
        if self.data.is_empty() { INF32 } else { unsafe { (*self.data.get_unchecked(0) >> 32) as u32 } }
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
                if packed >= parent { break; }
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
                    if c >= sz { break; }
                    let mut mc = c;
                    if c + 3 < sz {
                        if *slice.add(c + 1) < *slice.add(mc) { mc = c + 1; }
                        if *slice.add(c + 2) < *slice.add(mc) { mc = c + 2; }
                        if *slice.add(c + 3) < *slice.add(mc) { mc = c + 3; }
                    } else {
                        for k in c + 1..sz { if *slice.add(k) < *slice.add(mc) { mc = k; } }
                    }
                    if *slice.add(mc) >= last { break; }
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
    while **p >= b'0' { v = v * 10 + (**p - b'0') as u32; *p = p.add(1); }
    v
}

#[cold]
fn solve_query_64(
    s: usize, t: usize,
    hf_ptr: *const u32, hr_ptr: *const u32,
    ef_ptr: *const u32, er_ptr: *const u32,
    oflow: &[(u32, u32)],
) -> i64 {
    let mut dist_f = std::collections::HashMap::with_capacity(65536);
    let mut dist_b = std::collections::HashMap::with_capacity(65536);
    let mut q_f = std::collections::BinaryHeap::new();
    let mut q_b = std::collections::BinaryHeap::new();
    dist_f.insert(s as u32, 0u64); dist_b.insert(t as u32, 0u64);
    q_f.push((std::cmp::Reverse(0u64), s as u32)); q_b.push((std::cmp::Reverse(0u64), t as u32));
    let mut mu = u64::MAX;
    unsafe {
        while !q_f.is_empty() && !q_b.is_empty() {
            if q_f.peek().unwrap().0.0.saturating_add(q_b.peek().unwrap().0.0) >= mu { break; }
            let is_fwd = q_f.len() <= q_b.len();
            let (q, dist_m, dist_o, h_ptr, e_ptr, mask) = if is_fwd {
                (&mut q_f, &mut dist_f, &dist_b, hf_ptr, ef_ptr, 0u32)
            } else {
                (&mut q_b, &mut dist_b, &dist_f, hr_ptr, er_ptr, 1u32 << 31)
            };
            let (std::cmp::Reverse(d), u) = q.pop().unwrap();
            if d > *dist_m.get(&u).unwrap_or(&u64::MAX) { continue; }
            let (start, end) = (*h_ptr.add(u as usize) as usize, *h_ptr.add(u as usize + 1) as usize);
            for e in start..end {
                let pe = *e_ptr.add(e);
                let v = pe & V_MASK;
                let mut w = (pe >> V_BITS) as u64;
                if w == W_SENTINEL as u64 {
                    if let Ok(pos) = oflow.binary_search_by_key(&(mask | (e as u32)), |&(k, _)| k) {
                        w = oflow.get_unchecked(pos).1 as u64;
                    }
                }
                let nd = d + w;
                if nd < *dist_m.get(&v).unwrap_or(&u64::MAX) {
                    dist_m.insert(v, nd);
                    q.push((std::cmp::Reverse(nd), v));
                    if let Some(&opp_d) = dist_o.get(&v) { mu = mu.min(nd + opp_d); }
                }
            }
        }
    }
    if mu == u64::MAX { -1 } else { mu as i64 }
}

fn main() {
    let (data_dir, out_dir) = {
        let args: Vec<String> = std::env::args().collect();
        let (mut d, mut o) = ("/data".to_string(), "/out".to_string());
        let (mut i, mut pos) = (1, 0);
        while i < args.len() {
            if args[i] == "--data" && i + 1 < args.len() { d = args[i + 1].clone(); i += 2; }
            else if args[i] == "--out" && i + 1 < args.len() { o = args[i + 1].clone(); i += 2; }
            else if !args[i].starts_with('-') {
                if pos == 0 { d = args[i].clone(); } else { o = args[i].clone(); }
                pos += 1; i += 1;
            } else { i += 1; }
        }
        (d, o)
    };

    let gf = File::open(format!("{}/graph.txt", data_dir)).expect("graph.txt");
    let gsize = gf.metadata().expect("meta").len() as usize;

    let mut header_buf = [0u8; 64];
    let _ = gf.read_at(&mut header_buf, 0).expect("header read");
    let mut p = header_buf.as_ptr();
    let n = unsafe { parse_fast_u32(&mut p) };
    unsafe { p = p.add(1); }
    let _m = unsafe { parse_fast_u32(&mut p) };
    unsafe { if *p == b'\r' { p = p.add(1); } p = p.add(1); }
    let edge_start_offset = (p as usize) - (header_buf.as_ptr() as usize);

    let mut tail_buf = [0u8; 64];
    let tail_start = gsize.saturating_sub(64);
    let t_read = gf.read_at(&mut tail_buf, tail_start as u64).unwrap_or(0);
    let mut g_end = tail_start + t_read;
    while g_end > tail_start && tail_buf[g_end - tail_start - 1] < b'0' { g_end -= 1; }

    let nt = std::thread::available_parallelism().map(|v| v.get()).unwrap_or(12).min(12);
    let edge_bytes = g_end - edge_start_offset;
    let mut chunk_bounds = vec![g_end; nt + 1];
    chunk_bounds[0] = edge_start_offset;
    for t in 1..nt {
        let mut off = edge_start_offset + (edge_bytes * t) / nt;
        let mut probe = [0u8; 128];
        let pr = gf.read_at(&mut probe, off.saturating_sub(1) as u64).unwrap_or(0);
        let mut shift = 0;
        while shift + 1 < pr && off < g_end && probe[shift] != b'\n' { off += 1; shift += 1; }
        chunk_bounds[t] = off.min(g_end);
    }

    let head_fwd: Vec<AtomicU32> = unsafe { std::mem::transmute(vec![0u32; (n + 1) as usize]) };
    let head_rev: Vec<AtomicU32> = unsafe { std::mem::transmute(vec![0u32; (n + 1) as usize]) };

    std::thread::scope(|scope| {
        for t in 0..nt {
            let (start, finish) = (chunk_bounds[t], chunk_bounds[t + 1]);
            let (hf, hr, gf_ref) = (&head_fwd, &head_rev, &gf);
            scope.spawn(move || {
                let mut buf = vec![0u8; STREAM_BUF_SIZE + 64];
                let (mut file_off, mut leftover) = (start, 0usize);
                while file_off < finish {
                    let to_read = (finish - file_off).min(STREAM_BUF_SIZE);
                    let got = gf_ref.read_at(&mut buf[leftover..leftover + to_read], file_off as u64).unwrap_or(0);
                    if got == 0 { break; }
                    file_off += got;
                    let total = leftover + got;
                    let mut valid_end = total;
                    if file_off < finish {
                        while valid_end > 0 && buf[valid_end - 1] != b'\n' { valid_end -= 1; }
                    } else if valid_end == total { buf[total] = b'\n'; }
                    unsafe {
                        let (mut cur, end_cur) = (buf.as_ptr(), buf.as_ptr().add(valid_end));
                        while cur < end_cur {
                            let u = parse_fast_u32(&mut cur); cur = cur.add(1);
                            let v = parse_fast_u32(&mut cur);
                            while *cur != b'\n' { cur = cur.add(1); }
                            cur = cur.add(1);
                            if u != v {
                                hf.get_unchecked(u as usize).fetch_add(1, Ordering::Relaxed);
                                hr.get_unchecked(v as usize).fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                    leftover = total - valid_end;
                    if leftover > 0 { buf.copy_within(valid_end..total, 0); }
                }
            });
        }
    });

    let mut accum_fwd = 0u32;
    for a in &head_fwd { let d = a.load(Ordering::Relaxed); a.store(accum_fwd, Ordering::Relaxed); accum_fwd += d; }
    let mut accum_rev = 0u32;
    for a in &head_rev { let d = a.load(Ordering::Relaxed); a.store(accum_rev, Ordering::Relaxed); accum_rev += d; }

    let mut edges_fwd = vec![0u32; accum_fwd as usize];
    let mut edges_rev = vec![0u32; accum_rev as usize];
    let (ef_addr, er_addr) = (edges_fwd.as_mut_ptr() as usize, edges_rev.as_mut_ptr() as usize);

    let overflows: Vec<Vec<(u32, u32)>> = std::thread::scope(|scope| {
        let mut handlers = Vec::with_capacity(nt);
        for t in 0..nt {
            let (start, finish) = (chunk_bounds[t], chunk_bounds[t + 1]);
            let (hf, hr, gf_ref) = (&head_fwd, &head_rev, &gf);
            handlers.push(scope.spawn(move || {
                let mut local_overflow = Vec::new();
                let mut buf = vec![0u8; STREAM_BUF_SIZE + 64];
                let (mut file_off, mut leftover) = (start, 0usize);
                let (ef_ptr, er_ptr) = (ef_addr as *mut u32, er_addr as *mut u32);
                while file_off < finish {
                    let to_read = (finish - file_off).min(STREAM_BUF_SIZE);
                    let got = gf_ref.read_at(&mut buf[leftover..leftover + to_read], file_off as u64).unwrap_or(0);
                    if got == 0 { break; }
                    file_off += got;
                    let total = leftover + got;
                    let mut valid_end = total;
                    if file_off < finish {
                        while valid_end > 0 && buf[valid_end - 1] != b'\n' { valid_end -= 1; }
                    } else if valid_end == total { buf[total] = b'\n'; }
                    unsafe {
                        let (mut cur, end_cur) = (buf.as_ptr(), buf.as_ptr().add(valid_end));
                        while cur < end_cur {
                            let u = parse_fast_u32(&mut cur); cur = cur.add(1);
                            let v = parse_fast_u32(&mut cur); cur = cur.add(1);
                            let w = parse_fast_u32(&mut cur);
                            if *cur == b'\r' { cur = cur.add(1); }
                            cur = cur.add(1);
                            if u != v {
                                let pw = if w < W_SENTINEL { w } else { W_SENTINEL };
                                let idx_f = hf.get_unchecked(u as usize).fetch_add(1, Ordering::Relaxed) as usize;
                                *ef_ptr.add(idx_f) = (pw << V_BITS) | (v & V_MASK);
                                let idx_r = hr.get_unchecked(v as usize).fetch_add(1, Ordering::Relaxed) as usize;
                                *er_ptr.add(idx_r) = (pw << V_BITS) | (u & V_MASK);
                                if pw == W_SENTINEL {
                                    local_overflow.push((idx_f as u32, w));
                                    local_overflow.push(((1u32 << 31) | (idx_r as u32), w));
                                }
                            }
                        }
                    }
                    leftover = total - valid_end;
                    if leftover > 0 { buf.copy_within(valid_end..total, 0); }
                }
                local_overflow
            }));
        }
        handlers.into_iter().map(|h| h.join().unwrap()).collect()
    });

    drop(chunk_bounds);
    drop(gf);

    let mut overflow_map: Vec<(u32, u32)> = overflows.into_iter().flatten().collect();
    overflow_map.sort_unstable_by_key(|&(idx, _)| idx);

    let mut head_fwd: Vec<u32> = unsafe { std::mem::transmute(head_fwd) };
    let mut head_rev: Vec<u32> = unsafe { std::mem::transmute(head_rev) };
    if n > 0 { head_fwd.copy_within(0..n as usize, 1); head_rev.copy_within(0..n as usize, 1); }
    head_fwd[0] = 0; head_rev[0] = 0;

    let q_bytes = std::fs::read(format!("{}/queries.txt", data_dir)).expect("queries.txt");
    let mut queries = Vec::with_capacity(4096);
    let mut qp = q_bytes.as_ptr();
    let q_end_p = unsafe { qp.add(q_bytes.len()) };
    let mut qid = 0u32;
    unsafe {
        while qp < q_end_p {
            while qp < q_end_p && *qp < b'0' { qp = qp.add(1); }
            if qp >= q_end_p { break; }
            let s = parse_fast_u32(&mut qp);
            while qp < q_end_p && *qp < b'0' { qp = qp.add(1); }
            let t = parse_fast_u32(&mut qp);
            queries.push(Query { s, t, id: qid });
            qid += 1;
        }
    }
    drop(q_bytes);

    let mut answers = vec![-1i64; queries.len()];
    let answers_addr = answers.as_mut_ptr() as usize;
    let q_threads = nt.min(12).min(queries.len());
    let next_query = AtomicUsize::new(0);

    std::thread::scope(|scope| {
        for _ in 0..q_threads {
            let next_ref = &next_query;
            let q_ref = &queries;
            let (hf_ref, hr_ref) = (&head_fwd, &head_rev);
            let (ef_ref, er_ref) = (&edges_fwd, &edges_rev);
            let oflow_ref = &overflow_map;
            let has_overflow = !overflow_map.is_empty();

            scope.spawn(move || {
                let mut dist_map = FastDistMap::new();
                let mut heap_f = MinHeap4::with_capacity(32768);
                let mut heap_b = MinHeap4::with_capacity(32768);
                let (hf_ptr, hr_ptr) = (hf_ref.as_ptr(), hr_ref.as_ptr());
                let (ef_ptr, er_ptr) = (ef_ref.as_ptr(), er_ref.as_ptr());
                let ans_ptr = answers_addr as *mut i64;

                loop {
                    let qi = next_ref.fetch_add(1, Ordering::Relaxed);
                    if qi >= q_ref.len() { break; }
                    let q = q_ref[qi];
                    let (s, t) = (q.s as usize, q.t as usize);
                    if s == t { unsafe { *ans_ptr.add(q.id as usize) = 0; } continue; }

                    unsafe {
                        let sf_deg = *hf_ptr.add(s + 1) - *hf_ptr.add(s);
                        let tr_deg = *hr_ptr.add(t + 1) - *hr_ptr.add(t);
                        if sf_deg == 0 || tr_deg == 0 { *ans_ptr.add(q.id as usize) = -1; continue; }
                    }

                    dist_map.next_query();
                    heap_f.clear(); heap_b.clear();
                    unsafe {
                        dist_map.init_start(s as u32, t as u32);
                    }
                    heap_f.push(0, s as u32); heap_b.push(0, t as u32);

                    let mut mu = INF32;
                    let (mut touched_overflow, mut table_full) = (false, false);

                    unsafe {
                        while !heap_f.is_empty() && !heap_b.is_empty() {
                            if mu != INF32 && heap_f.min_key().saturating_add(heap_b.min_key()) >= mu { break; }
                            if dist_map.used >= H_MAX_LOAD { table_full = true; break; }

                            if heap_f.data.len() <= heap_b.data.len() {
                                let (d, u) = heap_f.pop();
                                if d > dist_map.get_fwd_dist(u) { continue; }
                                let (e_start, e_end) = (*hf_ptr.add(u as usize) as usize, *hf_ptr.add(u as usize + 1) as usize);
                                for e in e_start..e_end {
                                    let packed = *ef_ptr.add(e);
                                    let v = packed & V_MASK;
                                    let mut w = packed >> V_BITS;
                                    if has_overflow && w == W_SENTINEL {
                                        touched_overflow = true;
                                        if let Ok(pos) = oflow_ref.binary_search_by_key(&(e as u32), |&(k, _)| k) {
                                            w = oflow_ref.get_unchecked(pos).1;
                                        }
                                    }
                                    let nd = d.saturating_add(w);
                                    if nd >= mu { continue; }
                                    dist_map.relax_fwd(v, nd, &mut heap_f, &mut mu);
                                }
                            } else {
                                let (d, u) = heap_b.pop();
                                if d > dist_map.get_rev_dist(u) { continue; }
                                let (e_start, e_end) = (*hr_ptr.add(u as usize) as usize, *hr_ptr.add(u as usize + 1) as usize);
                                for e in e_start..e_end {
                                    let packed = *er_ptr.add(e);
                                    let v = packed & V_MASK;
                                    let mut w = packed >> V_BITS;
                                    if has_overflow && w == W_SENTINEL {
                                        touched_overflow = true;
                                        if let Ok(pos) = oflow_ref.binary_search_by_key(&((1u32 << 31) | (e as u32)), |&(k, _)| k) {
                                            w = oflow_ref.get_unchecked(pos).1;
                                        }
                                    }
                                    let nd = d.saturating_add(w);
                                    if nd >= mu { continue; }
                                    dist_map.relax_rev(v, nd, &mut heap_b, &mut mu);
                                }
                            }
                        }

                        let mut ans_val = if mu == INF32 { -1 } else { mu as i64 };
                        if table_full || (ans_val == -1 && touched_overflow) {
                            ans_val = solve_query_64(s, t, hf_ptr, hr_ptr, ef_ptr, er_ptr, oflow_ref);
                        }
                        *ans_ptr.add(q.id as usize) = ans_val;
                    }
                }
            });
        }
    });

    let _ = std::fs::create_dir_all(&out_dir);
    let mut f = File::create(format!("{}/distances.txt", out_dir)).expect("distances.txt");
    let mut out_buf = Vec::with_capacity(answers.len() * 12);
    for a in answers {
        let mut n = a;
        if n < 0 { out_buf.extend_from_slice(b"-1\n"); continue; }
        if n == 0 { out_buf.extend_from_slice(b"0\n"); continue; }
        let start = out_buf.len();
        while n > 0 { out_buf.push(b'0' + (n % 10) as u8); n /= 10; }
        out_buf[start..].reverse();
        out_buf.push(b'\n');
    }
    f.write_all(&out_buf).expect("write output");
}
