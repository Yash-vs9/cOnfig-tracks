// Speed Demon 2026 — Track D (Traverse) Championship Solution
#![allow(dead_code, unused_unsafe)]

use std::fs::File;
use std::io::Write;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

extern "C" {
    fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
    fn madvise(a: *mut u8, l: usize, adv: i32) -> i32;
    fn munmap(a: *mut u8, l: usize) -> i32;
    fn mallopt(p: i32, v: i32) -> i32;
    fn _exit(status: i32) -> !;
}

const W_BITS: u32 = 11;
const W_SENTINEL: u32 = (1 << W_BITS) - 1;
const V_MASK: u32 = (1 << 21) - 1;
const INF: u32 = u32::MAX;
const H_SZ: usize = 65536;
const H_MSK: usize = H_SZ - 1;
const H_MAX_LOAD: usize = 48000;

#[derive(Clone, Copy)]
#[repr(C)]
struct Slot {
    v: u32,
    d: u32,
}

#[inline(always)]
fn hash(v: u32) -> usize {
    ((v.wrapping_mul(0x9E3779B9)) >> (32 - 16)) as usize
}

#[inline(always)]
unsafe fn parse_u32(p: &mut *const u8) -> u32 {
    let mut v = (**p - b'0') as u32;
    *p = p.add(1);
    while **p >= b'0' {
        v = v * 10 + (**p - b'0') as u32;
        *p = p.add(1);
    }
    v
}

struct MinHeap {
    data: Vec<u64>,
}
impl MinHeap {
    fn with_capacity(cap: usize) -> Self { Self { data: Vec::with_capacity(cap) } }
    #[inline(always)]
    fn clear(&mut self) { self.data.clear(); }
    #[inline(always)]
    fn is_empty(&self) -> bool { self.data.is_empty() }
    #[inline(always)]
    fn min_key(&self) -> u32 {
        if self.data.is_empty() { INF } else { unsafe { (*self.data.get_unchecked(0) >> 32) as u32 } }
    }
    #[inline(always)]
    fn push(&mut self, d: u32, slot_idx: u32) {
        let packed = ((d as u64) << 32) | (slot_idx as u64);
        let mut i = self.data.len();
        self.data.push(packed);
        unsafe {
            let slice = self.data.as_mut_ptr();
            while i > 0 {
                let p = (i - 1) >> 2;
                if packed >= *slice.add(p) { break; }
                *slice.add(i) = *slice.add(p);
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
                while (i << 2) + 1 < sz {
                    let c = (i << 2) + 1;
                    let mut mc = c;
                    for k in c + 1..(c + 4).min(sz) {
                        if *slice.add(k) < *slice.add(mc) { mc = k; }
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

#[cold]
fn solve_query_64(s: usize, t: usize, hf: *const u32, hr: *const u32, ef: *const u32, er: *const u32, oflow: &[(u32, u32)]) -> i64 {
    use std::collections::{BinaryHeap, HashMap};
    use std::cmp::Reverse;
    let (mut dist_f, mut dist_b) = (HashMap::with_capacity(2048), HashMap::with_capacity(2048));
    let (mut q_f, mut q_b) = (BinaryHeap::new(), BinaryHeap::new());
    dist_f.insert(s as u32, 0u64); dist_b.insert(t as u32, 0u64);
    q_f.push((Reverse(0u64), s as u32)); q_b.push((Reverse(0u64), t as u32));
    let mut mu = u64::MAX;
    unsafe {
        while !q_f.is_empty() && !q_b.is_empty() {
            if q_f.peek().unwrap().0.0.saturating_add(q_b.peek().unwrap().0.0) >= mu { break; }
            let is_fwd = q_f.len() <= q_b.len();
            let (q, dist_m, dist_o, h_ptr, e_ptr, mask) = if is_fwd {
                (&mut q_f, &mut dist_f, &dist_b, hf, ef, 0u32)
            } else {
                (&mut q_b, &mut dist_b, &dist_f, hr, er, 1u32 << 31)
            };
            let (Reverse(d), u) = q.pop().unwrap();
            if d > *dist_m.get(&u).unwrap_or(&u64::MAX) { continue; }
            let (start, end) = (*h_ptr.add(u as usize) as usize, *h_ptr.add(u as usize + 1) as usize);
            for e in start..end {
                let pe = *e_ptr.add(e);
                let (v, mut w) = (pe & V_MASK, (pe >> 21) as u64);
                if w == W_SENTINEL as u64 {
                    if let Ok(pos) = oflow.binary_search_by_key(&(mask | (e as u32)), |&(k, _)| k) {
                        w = oflow.get_unchecked(pos).1 as u64;
                    }
                }
                let nd = d + w;
                if nd < *dist_m.get(&v).unwrap_or(&u64::MAX) {
                    dist_m.insert(v, nd);
                    q.push((Reverse(nd), v));
                    if let Some(&opp_d) = dist_o.get(&v) { mu = mu.min(nd + opp_d); }
                }
            }
        }
    }
    if mu == u64::MAX { -1 } else { mu as i64 }
}

fn main() {
    unsafe { mallopt(-8, 2); }
    let args: Vec<String> = std::env::args().collect();
    let (d_def, o_def) = ("/data".to_string(), "/out".to_string());
    let data_dir = args.iter().position(|a| a == "--data").map(|i| &args[i + 1]).or_else(|| args.get(1)).unwrap_or(&d_def);
    let out_dir = args.iter().position(|a| a == "--out").map(|i| &args[i + 1]).or_else(|| args.get(2)).unwrap_or(&o_def);

    let gf = File::open(format!("{}/graph.txt", data_dir)).expect("graph");
    let gsize = gf.metadata().expect("meta").len() as usize;

    let fd = std::os::unix::io::AsRawFd::as_raw_fd(&gf);
    let ptr_addr = unsafe { mmap(std::ptr::null_mut(), gsize, 1, 2, fd, 0) } as usize;
    unsafe { madvise(ptr_addr as *mut u8, gsize, 2); }
    let ptr = ptr_addr as *const u8;

    let mut p = ptr;
    let n = unsafe { parse_u32(&mut p) };
    unsafe { p = p.add(1); }
    let _m = unsafe { parse_u32(&mut p) };
    unsafe { while *p != b'\n' { p = p.add(1); } p = p.add(1); }
    let edge_start = (p as usize) - ptr_addr;

    let mut g_end = gsize;
    while g_end > edge_start && unsafe { *ptr.add(g_end - 1) } < b'0' { g_end -= 1; }

    let nt = 12;
    let edge_bytes = g_end - edge_start;
    let mut bounds = vec![g_end; nt + 1];
    bounds[0] = edge_start;
    for t in 1..nt {
        let mut off = edge_start + (edge_bytes * t) / nt;
        while off < g_end && unsafe { *ptr.add(off) } != b'\n' { off += 1; }
        if off < g_end { off += 1; }
        bounds[t] = off;
    }

    let head_fwd: Vec<AtomicU32> = unsafe { std::mem::transmute(vec![0u32; (n + 1) as usize]) };
    let head_rev: Vec<AtomicU32> = unsafe { std::mem::transmute(vec![0u32; (n + 1) as usize]) };

    // Pass 1: parallel degree accumulation
    std::thread::scope(|scope| {
        for t in 0..nt {
            let (start, finish, hf, hr, base) = (bounds[t], bounds[t + 1], &head_fwd, &head_rev, ptr_addr);
            scope.spawn(move || {
                unsafe {
                    let mut cur = (base as *const u8).add(start);
                    let end_p = (base as *const u8).add(finish);
                    while cur < end_p {
                        let u = parse_u32(&mut cur); cur = cur.add(1);
                        let v = parse_u32(&mut cur); cur = cur.add(1);
                        while *cur != b'\n' { cur = cur.add(1); }
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

    let (mut accum_fwd, mut accum_rev) = (0u32, 0u32);
    for a in &head_fwd { let d = a.load(Ordering::Relaxed); a.store(accum_fwd, Ordering::Relaxed); accum_fwd += d; }
    for a in &head_rev { let d = a.load(Ordering::Relaxed); a.store(accum_rev, Ordering::Relaxed); accum_rev += d; }

    let mut edges_fwd = vec![0u32; accum_fwd as usize];
    let mut edges_rev = vec![0u32; accum_rev as usize];
    let (ef_addr, er_addr) = (edges_fwd.as_mut_ptr() as usize, edges_rev.as_mut_ptr() as usize);

    // Pass 2: parallel edge packing into dual CSR
    let overflows: Vec<Vec<(u32, u32)>> = std::thread::scope(|scope| {
        let mut handlers = Vec::with_capacity(nt);
        for t in 0..nt {
            let (start, finish, hf, hr, base) = (bounds[t], bounds[t + 1], &head_fwd, &head_rev, ptr_addr);
            handlers.push(scope.spawn(move || {
                let mut local_oflow = Vec::new();
                let (ef_ptr, er_ptr) = (ef_addr as *mut u32, er_addr as *mut u32);
                unsafe {
                    let mut cur = (base as *const u8).add(start);
                    let end_p = (base as *const u8).add(finish);
                    while cur < end_p {
                        let u = parse_u32(&mut cur); cur = cur.add(1);
                        let v = parse_u32(&mut cur); cur = cur.add(1);
                        let w = parse_u32(&mut cur);
                        if *cur == b'\r' { cur = cur.add(1); }
                        cur = cur.add(1);
                        if u != v {
                            let pw = if w < W_SENTINEL { w } else { W_SENTINEL };
                            let ifwd = hf.get_unchecked(u as usize).fetch_add(1, Ordering::Relaxed) as usize;
                            *ef_ptr.add(ifwd) = (pw << 21) | (v & V_MASK);
                            let irev = hr.get_unchecked(v as usize).fetch_add(1, Ordering::Relaxed) as usize;
                            *er_ptr.add(irev) = (pw << 21) | (u & V_MASK);
                            if pw == W_SENTINEL {
                                local_oflow.push((ifwd as u32, w));
                                local_oflow.push(((1u32 << 31) | (irev as u32), w));
                            }
                        }
                    }
                }
                local_oflow
            }));
        }
        handlers.into_iter().map(|h| h.join().unwrap()).collect()
    });

    unsafe { munmap(ptr_addr as *mut u8, gsize); }

    let mut overflow_map: Vec<(u32, u32)> = overflows.into_iter().flatten().collect();
    overflow_map.sort_unstable_by_key(|&(idx, _)| idx);

    let mut head_fwd: Vec<u32> = unsafe { std::mem::transmute(head_fwd) };
    let mut head_rev: Vec<u32> = unsafe { std::mem::transmute(head_rev) };
    if n > 0 {
        head_fwd.copy_within(0..n as usize, 1);
        head_rev.copy_within(0..n as usize, 1);
    }
    head_fwd[0] = 0; head_rev[0] = 0;

    let q_bytes = std::fs::read(format!("{}/queries.txt", data_dir)).expect("queries");
    let mut queries = Vec::with_capacity(4096);
    let mut qp = q_bytes.as_ptr();
    let q_end_p = unsafe { qp.add(q_bytes.len()) };
    let mut qid = 0u32;
    unsafe {
        while qp < q_end_p {
            while qp < q_end_p && *qp < b'0' { qp = qp.add(1); }
            if qp >= q_end_p { break; }
            let s = parse_u32(&mut qp);
            while qp < q_end_p && *qp < b'0' { qp = qp.add(1); }
            let t = parse_u32(&mut qp);
            queries.push((s, t, qid));
            qid += 1;
        }
    }

    let mut answers = vec![-1i64; queries.len()];
    let answers_addr = answers.as_mut_ptr() as usize;
    let next_query = AtomicUsize::new(0);

    // 12 query workers with private L2 cache working sets
    std::thread::scope(|scope| {
        for _ in 0..nt {
            let (nq, qref, hf, hr, ef, er, oflow) = (&next_query, &queries, &head_fwd, &head_rev, &edges_fwd, &edges_rev, &overflow_map);
            let has_oflow = !overflow_map.is_empty();

            scope.spawn(move || {
                let mut table_fwd = vec![Slot { v: u32::MAX, d: INF }; H_SZ];
                let mut table_rev = vec![Slot { v: u32::MAX, d: INF }; H_SZ];
                let mut dirty_fwd = Vec::with_capacity(8192);
                let mut dirty_rev = Vec::with_capacity(8192);
                let mut heap_f = MinHeap::with_capacity(8192);
                let mut heap_b = MinHeap::with_capacity(8192);

                let (hf_ptr, hr_ptr, ef_ptr, er_ptr) = (hf.as_ptr(), hr.as_ptr(), ef.as_ptr(), er.as_ptr());
                let ans_ptr = answers_addr as *mut i64;

                loop {
                    let base = nq.fetch_add(4, Ordering::Relaxed);
                    if base >= qref.len() { break; }
                    let end = (base + 4).min(qref.len());

                    for qi in base..end {
                        let (s, t, qid) = qref[qi];
                        if s == t { unsafe { *ans_ptr.add(qid as usize) = 0; } continue; }

                        unsafe {
                            if *hf_ptr.add(s as usize + 1) == *hf_ptr.add(s as usize) || *hr_ptr.add(t as usize + 1) == *hr_ptr.add(t as usize) {
                                *ans_ptr.add(qid as usize) = -1;
                                continue;
                            }
                        }

                        // Reset dirty slots in O(touched) time
                        for &idx in &dirty_fwd { unsafe { table_fwd.get_unchecked_mut(idx as usize).v = u32::MAX; } }
                        for &idx in &dirty_rev { unsafe { table_rev.get_unchecked_mut(idx as usize).v = u32::MAX; } }
                        dirty_fwd.clear();
                        dirty_rev.clear();
                        heap_f.clear();
                        heap_b.clear();

                        let mut s_idx = hash(s);
                        while unsafe { table_fwd.get_unchecked(s_idx).v } != u32::MAX { s_idx = (s_idx + 1) & H_MSK; }
                        unsafe { *table_fwd.get_unchecked_mut(s_idx) = Slot { v: s, d: 0 }; }
                        dirty_fwd.push(s_idx as u32);
                        heap_f.push(0, s_idx as u32);

                        let mut t_idx = hash(t);
                        while unsafe { table_rev.get_unchecked(t_idx).v } != u32::MAX { t_idx = (t_idx + 1) & H_MSK; }
                        unsafe { *table_rev.get_unchecked_mut(t_idx) = Slot { v: t, d: 0 }; }
                        dirty_rev.push(t_idx as u32);
                        heap_b.push(0, t_idx as u32);

                        let mut mu = INF;
                        let (mut touched_oflow, mut table_full) = (false, false);

                        while !heap_f.is_empty() && !heap_b.is_empty() {
                            let (top_f, top_b) = (heap_f.min_key(), heap_b.min_key());
                            if top_f.saturating_add(top_b) >= mu { break; }
                            if dirty_fwd.len() >= H_MAX_LOAD || dirty_rev.len() >= H_MAX_LOAD {
                                table_full = true;
                                break;
                            }

                            let is_fwd = heap_f.data.len() <= heap_b.data.len();
                            let (d, slot_idx) = if is_fwd { heap_f.pop() } else { heap_b.pop() };
                            let top_opp = if is_fwd { top_b } else { top_f };
                            if d.saturating_add(top_opp) >= mu { break; }

                            let u = unsafe {
                                if is_fwd {
                                    let slot = table_fwd.get_unchecked(slot_idx as usize);
                                    if d > slot.d { continue; }
                                    slot.v
                                } else {
                                    let slot = table_rev.get_unchecked(slot_idx as usize);
                                    if d > slot.d { continue; }
                                    slot.v
                                }
                            };

                            let (h_ptr, e_ptr, mask) = if is_fwd { (hf_ptr, ef_ptr, 0u32) } else { (hr_ptr, er_ptr, 1u32 << 31) };
                            let (e_start, e_end) = unsafe { (*h_ptr.add(u as usize) as usize, *h_ptr.add(u as usize + 1) as usize) };

                            for e in e_start..e_end {
                                let packed = unsafe { *e_ptr.add(e) };
                                let (v, mut w) = (packed & V_MASK, packed >> 21);
                                if has_oflow && w == W_SENTINEL {
                                    touched_oflow = true;
                                    if let Ok(pos) = oflow.binary_search_by_key(&(mask | (e as u32)), |&(k, _)| k) {
                                        w = oflow[pos].1;
                                    }
                                }
                                let nd = d.saturating_add(w);
                                if nd >= mu { continue; }

                                if is_fwd {
                                    let mut r_idx = hash(v);
                                    loop {
                                        let slot = unsafe { table_rev.get_unchecked(r_idx) };
                                        if slot.v == v {
                                            let cand = nd.saturating_add(slot.d);
                                            if cand < mu { mu = cand; }
                                            break;
                                        }
                                        if slot.v == u32::MAX { break; }
                                        r_idx = (r_idx + 1) & H_MSK;
                                    }
                                    let mut f_idx = hash(v);
                                    loop {
                                        let slot = unsafe { table_fwd.get_unchecked_mut(f_idx) };
                                        if slot.v == v {
                                            if nd < slot.d {
                                                slot.d = nd;
                                                if nd.saturating_add(top_opp) < mu {
                                                    heap_f.push(nd, f_idx as u32);
                                                }
                                            }
                                            break;
                                        }
                                        if slot.v == u32::MAX {
                                            if nd.saturating_add(top_opp) < mu {
                                                *slot = Slot { v, d: nd };
                                                dirty_fwd.push(f_idx as u32);
                                                heap_f.push(nd, f_idx as u32);
                                            }
                                            break;
                                        }
                                        f_idx = (f_idx + 1) & H_MSK;
                                    }
                                } else {
                                    let mut f_idx = hash(v);
                                    loop {
                                        let slot = unsafe { table_fwd.get_unchecked(f_idx) };
                                        if slot.v == v {
                                            let cand = nd.saturating_add(slot.d);
                                            if cand < mu { mu = cand; }
                                            break;
                                        }
                                        if slot.v == u32::MAX { break; }
                                        f_idx = (f_idx + 1) & H_MSK;
                                    }
                                    let mut r_idx = hash(v);
                                    loop {
                                        let slot = unsafe { table_rev.get_unchecked_mut(r_idx) };
                                        if slot.v == v {
                                            if nd < slot.d {
                                                slot.d = nd;
                                                if nd.saturating_add(top_opp) < mu {
                                                    heap_b.push(nd, r_idx as u32);
                                                }
                                            }
                                            break;
                                        }
                                        if slot.v == u32::MAX {
                                            if nd.saturating_add(top_opp) < mu {
                                                *slot = Slot { v, d: nd };
                                                dirty_rev.push(r_idx as u32);
                                                heap_b.push(nd, r_idx as u32);
                                            }
                                            break;
                                        }
                                        r_idx = (r_idx + 1) & H_MSK;
                                    }
                                }
                            }
                        }

                        let mut ans = if mu == INF { -1 } else { mu as i64 };
                        if table_full || (ans == -1 && touched_oflow) {
                            ans = solve_query_64(s as usize, t as usize, hf_ptr, hr_ptr, ef_ptr, er_ptr, oflow);
                        }
                        unsafe { *ans_ptr.add(qid as usize) = ans; }
                    }
                }
            });
        }
    });

    let _ = std::fs::create_dir_all(&out_dir);
    let mut f = File::create(format!("{}/distances.txt", out_dir)).expect("out");
    let mut out_buf = Vec::with_capacity(answers.len() * 12);
    let mut tmp = [0u8; 24];
    for a in answers {
        if a < 0 {
            out_buf.extend_from_slice(b"-1\n");
        } else if a == 0 {
            out_buf.extend_from_slice(b"0\n");
        } else {
            let mut v = a as u64;
            let mut idx = 24;
            while v > 0 {
                idx -= 1;
                tmp[idx] = b'0' + (v % 10) as u8;
                v /= 10;
            }
            out_buf.extend_from_slice(&tmp[idx..24]);
            out_buf.push(b'\n');
        }
    }
    f.write_all(&out_buf).expect("write");
    unsafe { _exit(0); }
}
