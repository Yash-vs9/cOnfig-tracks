use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::fs::File;
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::{_mm_prefetch, _MM_HINT_T0};

const W_BITS: u32 = 11;
const W_SENTINEL: u32 = (1 << W_BITS) - 1;
const V_MASK: u32 = (1 << 21) - 1;
const INF: u32 = u32::MAX;
const H_SZ: usize = 65536;
const H_MSK: usize = H_SZ - 1;
const H_SHIFT: u32 = 32 - 16;
const H_MAX_LOAD: u32 = 58000;
const BUF_SZ: usize = 128 * 1024;

#[derive(Clone, Copy)]
struct Query(u32, u32, u32);

#[derive(Clone, Copy, Default)]
#[repr(C, align(16))]
struct Slot(u32, u32, u32, u32);

struct FastDistMap {
    slots: Vec<Slot>,
    epoch: u32,
    used: u32,
}
impl FastDistMap {
    fn new() -> Self {
        Self {
            slots: unsafe { std::mem::transmute(vec![0u128; H_SZ]) },
            epoch: 1,
            used: 0,
        }
    }
    #[inline(always)]
    fn next_query(&mut self) {
        self.used = 0;
        self.epoch = self.epoch.wrapping_add(1);
        if self.epoch == 0 {
            for s in &mut self.slots {
                s.3 = 0;
            }
            self.epoch = 1;
        }
    }
    #[inline(always)]
    unsafe fn init_start(&mut self, s: u32, t: u32) -> (u32, u32) {
        let ep = self.epoch;
        let base = self.slots.as_mut_ptr();
        let mut idx_s = (s.wrapping_mul(0x9E3779B9) >> H_SHIFT) as usize;
        loop {
            let slot = base.add(idx_s);
            if (*slot).3 != ep || (*slot).0 == s {
                *slot = Slot(s, 0, INF, ep);
                self.used += 1;
                break;
            }
            idx_s = (idx_s + 1) & H_MSK;
        }
        let mut idx_t = (t.wrapping_mul(0x9E3779B9) >> H_SHIFT) as usize;
        loop {
            let slot = base.add(idx_t);
            if (*slot).3 != ep || (*slot).0 == t {
                *slot = Slot(t, INF, 0, ep);
                self.used += 1;
                break;
            }
            idx_t = (idx_t + 1) & H_MSK;
        }
        (idx_s as u32, idx_t as u32)
    }
    #[inline(always)]
    unsafe fn prefetch(&self, v: u32) {
        let idx = (v.wrapping_mul(0x9E3779B9) >> H_SHIFT) as usize;
        _mm_prefetch(self.slots.as_ptr().add(idx) as *const i8, _MM_HINT_T0);
    }
    #[inline(always)]
    unsafe fn relax<const IS_FWD: bool>(
        &mut self,
        v: u32,
        nd: u32,
        top_opp: u32,
        heap: &mut MinHeap,
        mu: &mut u32,
    ) {
        let ep = self.epoch;
        let mut idx = (v.wrapping_mul(0x9E3779B9) >> H_SHIFT) as usize;
        let base = self.slots.as_mut_ptr();
        loop {
            let slot = base.add(idx);
            if (*slot).3 != ep {
                if nd.saturating_add(top_opp) < *mu {
                    let (f, r) = if IS_FWD { (nd, INF) } else { (INF, nd) };
                    *slot = Slot(v, f, r, ep);
                    self.used += 1;
                    heap.push(nd, idx as u32);
                }
                return;
            }
            if (*slot).0 == v {
                let my_d = if IS_FWD {
                    &mut (*slot).1
                } else {
                    &mut (*slot).2
                };
                if nd < *my_d {
                    let opp_d = if IS_FWD { (*slot).2 } else { (*slot).1 };
                    if opp_d != INF {
                        *mu = (*mu).min(nd.saturating_add(opp_d));
                    }
                    if nd.saturating_add(top_opp) < *mu {
                        *my_d = nd;
                        heap.push(nd, idx as u32);
                    }
                }
                return;
            }
            idx = (idx + 1) & H_MSK;
        }
    }
}

struct MinHeap {
    data: Vec<u64>,
}
impl MinHeap {
    fn new(cap: usize) -> Self {
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
            INF
        } else {
            unsafe { (*self.data.get_unchecked(0) >> 32) as u32 }
        }
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
unsafe fn parse_u32(p: &mut *const u8) -> u32 {
    let mut v = (**p - b'0') as u32;
    *p = p.add(1);
    while **p >= b'0' {
        v = v * 10 + (**p - b'0') as u32;
        *p = p.add(1);
    }
    v
}

fn stream_file<F: FnMut(&[u8])>(gf: &File, mut off: usize, finish: usize, mut on_chunk: F) {
    let mut buf = vec![0u8; BUF_SZ + 64];
    let mut left = 0usize;
    while off < finish {
        let to_read = (finish - off).min(BUF_SZ);
        let got = gf
            .read_at(&mut buf[left..left + to_read], off as u64)
            .unwrap_or(0);
        if got == 0 {
            break;
        }
        off += got;
        let total = left + got;
        let mut vend = total;
        if off < finish {
            while vend > 0 && buf[vend - 1] != b'\n' {
                vend -= 1;
            }
        } else if vend == total {
            buf[total] = b'\n';
        }
        on_chunk(&buf[..vend]);
        left = total - vend;
        if left > 0 {
            buf.copy_within(vend..total, 0);
        }
    }
}

#[cold]
fn solve_query_64(
    s: usize,
    t: usize,
    hf: *const u32,
    hr: *const u32,
    ef: *const u32,
    er: *const u32,
    oflow: &[(u32, u32)],
) -> i64 {
    let (mut dist_f, mut dist_b) = (HashMap::with_capacity(32768), HashMap::with_capacity(32768));
    let (mut q_f, mut q_b) = (BinaryHeap::new(), BinaryHeap::new());
    dist_f.insert(s as u32, 0u64);
    dist_b.insert(t as u32, 0u64);
    q_f.push((Reverse(0u64), s as u32));
    q_b.push((Reverse(0u64), t as u32));
    let mut mu = u64::MAX;
    unsafe {
        while !q_f.is_empty() && !q_b.is_empty() {
            let (top_f, top_b) = (q_f.peek().unwrap().0 .0, q_b.peek().unwrap().0 .0);
            if top_f.saturating_add(top_b) >= mu {
                break;
            }
            let is_fwd = q_f.len() <= q_b.len();
            let (q, dist_m, dist_o, h_ptr, e_ptr, mask) = if is_fwd {
                (&mut q_f, &mut dist_f, &dist_b, hf, ef, 0u32)
            } else {
                (&mut q_b, &mut dist_b, &dist_f, hr, er, 1u32 << 31)
            };
            let (Reverse(d), u) = q.pop().unwrap();
            if d > *dist_m.get(&u).unwrap_or(&u64::MAX) {
                continue;
            }
            let (start, end) = (
                *h_ptr.add(u as usize) as usize,
                *h_ptr.add(u as usize + 1) as usize,
            );
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
                    if let Some(&opp_d) = dist_o.get(&v) {
                        mu = mu.min(nd + opp_d);
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
    let args: Vec<String> = std::env::args().collect();
    let mut data_dir = "/data";
    let mut out_dir = "/out";
    for i in 1..args.len() {
        if args[i] == "--data" && i + 1 < args.len() {
            data_dir = &args[i + 1];
        }
        if args[i] == "--out" && i + 1 < args.len() {
            out_dir = &args[i + 1];
        }
    }
    if data_dir == "/data" && args.len() > 1 && !args[1].starts_with('-') {
        data_dir = &args[1];
    }
    if out_dir == "/out" && args.len() > 2 && !args[2].starts_with('-') {
        out_dir = &args[2];
    }

    let gf = File::open(format!("{}/graph.txt", data_dir)).expect("graph");
    let gsize = gf.metadata().expect("meta").len() as usize;

    let mut header = [0u8; 64];
    let _ = gf.read_at(&mut header, 0);
    let mut p = header.as_ptr();
    let n = unsafe { parse_u32(&mut p) };
    unsafe {
        p = p.add(1);
    }
    let _m = unsafe { parse_u32(&mut p) };
    unsafe {
        if *p == b'\r' {
            p = p.add(1);
        }
        p = p.add(1);
    }
    let edge_start = (p as usize) - (header.as_ptr() as usize);

    let mut tail = [0u8; 64];
    let tail_start = gsize.saturating_sub(64);
    let tr = gf.read_at(&mut tail, tail_start as u64).unwrap_or(0);
    let mut g_end = tail_start + tr;
    while g_end > tail_start && tail[g_end - tail_start - 1] < b'0' {
        g_end -= 1;
    }

    let nt = 12;
    let edge_bytes = g_end - edge_start;
    let mut bounds = vec![g_end; nt + 1];
    bounds[0] = edge_start;
    for t in 1..nt {
        let mut off = edge_start + (edge_bytes * t) / nt;
        let mut probe = [0u8; 128];
        let pr = gf
            .read_at(&mut probe, off.saturating_sub(1) as u64)
            .unwrap_or(0);
        let mut s = 0;
        while s + 1 < pr && off < g_end && probe[s] != b'\n' {
            off += 1;
            s += 1;
        }
        bounds[t] = off.min(g_end);
    }

    let head_fwd: Vec<AtomicU32> = unsafe { std::mem::transmute(vec![0u32; (n + 1) as usize]) };
    let head_rev: Vec<AtomicU32> = unsafe { std::mem::transmute(vec![0u32; (n + 1) as usize]) };

    std::thread::scope(|scope| {
        for t in 0..nt {
            let (start, finish, hf, hr, gf_ref) =
                (bounds[t], bounds[t + 1], &head_fwd, &head_rev, &gf);
            scope.spawn(move || {
                stream_file(gf_ref, start, finish, |chunk| unsafe {
                    let (mut cur, end_p) = (chunk.as_ptr(), chunk.as_ptr().add(chunk.len()));
                    while cur < end_p {
                        let u = parse_u32(&mut cur);
                        cur = cur.add(1);
                        let v = parse_u32(&mut cur);
                        while *cur != b'\n' {
                            cur = cur.add(1);
                        }
                        cur = cur.add(1);
                        if u != v {
                            hf.get_unchecked(u as usize).fetch_add(1, Ordering::Relaxed);
                            hr.get_unchecked(v as usize).fetch_add(1, Ordering::Relaxed);
                        }
                    }
                });
            });
        }
    });

    let (mut accum_fwd, mut accum_rev) = (0u32, 0u32);
    for a in &head_fwd {
        let d = a.load(Ordering::Relaxed);
        a.store(accum_fwd, Ordering::Relaxed);
        accum_fwd += d;
    }
    for a in &head_rev {
        let d = a.load(Ordering::Relaxed);
        a.store(accum_rev, Ordering::Relaxed);
        accum_rev += d;
    }

    let mut edges_fwd = vec![0u32; accum_fwd as usize];
    let mut edges_rev = vec![0u32; accum_rev as usize];
    let (ef_addr, er_addr) = (
        edges_fwd.as_mut_ptr() as usize,
        edges_rev.as_mut_ptr() as usize,
    );

    let overflows: Vec<Vec<(u32, u32)>> = std::thread::scope(|scope| {
        let mut handlers = Vec::with_capacity(nt);
        for t in 0..nt {
            let (start, finish, hf, hr, gf_ref) =
                (bounds[t], bounds[t + 1], &head_fwd, &head_rev, &gf);
            handlers.push(scope.spawn(move || {
                let mut local_oflow = Vec::new();
                let (ef_ptr, er_ptr) = (ef_addr as *mut u32, er_addr as *mut u32);
                stream_file(gf_ref, start, finish, |chunk| unsafe {
                    let (mut cur, end_p) = (chunk.as_ptr(), chunk.as_ptr().add(chunk.len()));
                    while cur < end_p {
                        let u = parse_u32(&mut cur);
                        cur = cur.add(1);
                        let v = parse_u32(&mut cur);
                        cur = cur.add(1);
                        let w = parse_u32(&mut cur);
                        if *cur == b'\r' {
                            cur = cur.add(1);
                        }
                        cur = cur.add(1);
                        if u != v {
                            let pw = if w < W_SENTINEL { w } else { W_SENTINEL };
                            let ifwd = hf.get_unchecked(u as usize).fetch_add(1, Ordering::Relaxed)
                                as usize;
                            *ef_ptr.add(ifwd) = (pw << 21) | (v & V_MASK);
                            let irev = hr.get_unchecked(v as usize).fetch_add(1, Ordering::Relaxed)
                                as usize;
                            *er_ptr.add(irev) = (pw << 21) | (u & V_MASK);
                            if pw == W_SENTINEL {
                                local_oflow.push((ifwd as u32, w));
                                local_oflow.push(((1u32 << 31) | (irev as u32), w));
                            }
                        }
                    }
                });
                local_oflow
            }));
        }
        handlers.into_iter().map(|h| h.join().unwrap()).collect()
    });

    drop(bounds);
    drop(gf);

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

    let q_bytes = std::fs::read(format!("{}/queries.txt", data_dir)).expect("queries");
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
            let s = parse_u32(&mut qp);
            while qp < q_end_p && *qp < b'0' {
                qp = qp.add(1);
            }
            let t = parse_u32(&mut qp);
            queries.push(Query(s, t, qid));
            qid += 1;
        }
    }
    drop(q_bytes);

    queries.sort_unstable_by_key(|q| (q.0, q.1));
    let mut unique_queries: Vec<Query> = Vec::with_capacity(queries.len());
    let mut duplicates = Vec::new();
    for q in queries {
        if let Some(prev) = unique_queries.last() {
            if prev.0 == q.0 && prev.1 == q.1 {
                duplicates.push((q.2, prev.2));
                continue;
            }
        }
        unique_queries.push(q);
    }

    let mut answers = vec![-1i64; unique_queries.len() + duplicates.len()];
    let answers_addr = answers.as_mut_ptr() as usize;
    let next_query = AtomicUsize::new(0);

    let n_workers = nt.min(unique_queries.len());
    std::thread::scope(|scope| {
        for _ in 0..n_workers {
            let (nq, qref, hf, hr, ef, er, oflow) = (
                &next_query,
                &unique_queries,
                &head_fwd,
                &head_rev,
                &edges_fwd,
                &edges_rev,
                &overflow_map,
            );
            let has_oflow = !overflow_map.is_empty();

            scope.spawn(move || {
                let mut dist_map = FastDistMap::new();
                let mut heap_f = MinHeap::new(16384);
                let mut heap_b = MinHeap::new(16384);
                let (hf_ptr, hr_ptr, ef_ptr, er_ptr) =
                    (hf.as_ptr(), hr.as_ptr(), ef.as_ptr(), er.as_ptr());
                let ans_ptr = answers_addr as *mut i64;

                loop {
                    let qi = nq.fetch_add(1, Ordering::Relaxed);
                    if qi >= qref.len() {
                        break;
                    }
                    let q = qref[qi];
                    let (s, t) = (q.0 as usize, q.1 as usize);
                    if s == t {
                        unsafe {
                            *ans_ptr.add(q.2 as usize) = 0;
                        }
                        continue;
                    }

                    unsafe {
                        if *hf_ptr.add(s + 1) == *hf_ptr.add(s)
                            || *hr_ptr.add(t + 1) == *hr_ptr.add(t)
                        {
                            *ans_ptr.add(q.2 as usize) = -1;
                            continue;
                        }
                    }

                    dist_map.next_query();
                    heap_f.clear();
                    heap_b.clear();
                    let (s_idx, t_idx) = unsafe { dist_map.init_start(s as u32, t as u32) };
                    heap_f.push(0, s_idx);
                    heap_b.push(0, t_idx);

                    let mut mu = INF;
                    let (mut touched_oflow, mut table_full) = (false, false);

                    unsafe {
                        while !heap_f.is_empty() && !heap_b.is_empty() {
                            let (top_f, top_b) = (heap_f.min_key(), heap_b.min_key());
                            if top_f.saturating_add(top_b) >= mu {
                                break;
                            }
                            if dist_map.used >= H_MAX_LOAD {
                                table_full = true;
                                break;
                            }

                            let is_fwd = heap_f.data.len() <= heap_b.data.len();
                            let (d, slot_idx) = if is_fwd { heap_f.pop() } else { heap_b.pop() };
                            let top_opp = if is_fwd { top_b } else { top_f };
                            if d.saturating_add(top_opp) >= mu {
                                break;
                            }

                            let slot = dist_map.slots.as_ptr().add(slot_idx as usize);
                            let u = (*slot).0;
                            let my_d = if is_fwd { (*slot).1 } else { (*slot).2 };
                            if d > my_d {
                                continue;
                            }

                            let (h_ptr, e_ptr, mask) = if is_fwd {
                                (hf_ptr, ef_ptr, 0u32)
                            } else {
                                (hr_ptr, er_ptr, 1u32 << 31)
                            };
                            let (e_start, e_end) = (
                                *h_ptr.add(u as usize) as usize,
                                *h_ptr.add(u as usize + 1) as usize,
                            );

                            for e in e_start..e_end {
                                if dist_map.used >= H_MAX_LOAD {
                                    table_full = true;
                                    break;
                                }
                                let packed = *e_ptr.add(e);
                                let (v, mut w) = (packed & V_MASK, packed >> 21);
                                if e + 1 < e_end {
                                    dist_map.prefetch(*e_ptr.add(e + 1) & V_MASK);
                                }
                                if has_oflow && w == W_SENTINEL {
                                    touched_oflow = true;
                                    if let Ok(pos) = oflow
                                        .binary_search_by_key(&(mask | (e as u32)), |&(k, _)| k)
                                    {
                                        w = oflow.get_unchecked(pos).1;
                                    }
                                }
                                let nd = d.saturating_add(w);
                                if nd >= mu {
                                    continue;
                                }
                                if is_fwd {
                                    dist_map.relax::<true>(v, nd, top_opp, &mut heap_f, &mut mu);
                                } else {
                                    dist_map.relax::<false>(v, nd, top_opp, &mut heap_b, &mut mu);
                                }
                            }
                            if table_full {
                                break;
                            }
                        }

                        let mut ans = if mu == INF { -1 } else { mu as i64 };
                        if table_full || (ans == -1 && touched_oflow) {
                            ans = solve_query_64(s, t, hf_ptr, hr_ptr, ef_ptr, er_ptr, oflow);
                        }
                        *ans_ptr.add(q.2 as usize) = ans;
                    }
                }
            });
        }
    });

    for &(tid, sid) in &duplicates {
        answers[tid as usize] = answers[sid as usize];
    }

    let _ = std::fs::create_dir_all(out_dir);
    let mut f = File::create(format!("{}/distances.txt", out_dir)).expect("out");
    let mut out_buf = Vec::with_capacity(answers.len() * 12);
    for a in answers {
        let _ = writeln!(out_buf, "{}", a);
    }
    f.write_all(&out_buf).expect("write");
}
