
#[cold]
fn solve_query_64(
    s: usize,
    t: usize,
    hf: &[u32],
    hr: &[u32],
    ef: &[u32],
    er: &[u32],
    oflow: &[(u32, u64)],
) -> i64 {
    let (mut dist_f, mut dist_b) = (std::collections::HashMap::with_capacity(2048), std::collections::HashMap::with_capacity(2048));
    let (mut q_f, mut q_b) = (std::collections::BinaryHeap::new(), std::collections::BinaryHeap::new());
    dist_f.insert(s as u32, 0u64);
    dist_b.insert(t as u32, 0u64);
    q_f.push((std::cmp::Reverse(0u64), s as u32));
    q_b.push((std::cmp::Reverse(0u64), t as u32));
    let mut mu = u64::MAX;
    unsafe {
        while !q_f.is_empty() && !q_b.is_empty() {
            if q_f.peek().unwrap().0 .0.saturating_add(q_b.peek().unwrap().0 .0) >= mu {
                break;
            }
            let is_fwd = q_f.len() <= q_b.len();
            let (q, dist_m, dist_o, h_ptr, e_ptr, mask) = if is_fwd {
                (&mut q_f, &mut dist_f, &dist_b, hf.as_ptr(), ef.as_ptr(), 0u32)
            } else {
                (&mut q_b, &mut dist_b, &dist_f, hr.as_ptr(), er.as_ptr(), 1u32 << 31)
            };
            let (std::cmp::Reverse(d), u) = q.pop().unwrap();
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
                if w == 2047 {
                    if let Ok(pos) = oflow.binary_search_by_key(&(mask | (e as u32)), |&(k, _)| k) {
                        w = oflow.get_unchecked(pos).1;
                    }
                }
                let nd = d + w;
                if nd < *dist_m.get(&v).unwrap_or(&u64::MAX) {
                    dist_m.insert(v, nd);
                    q.push((std::cmp::Reverse(nd), v));
                    if let Some(&opp_d) = dist_o.get(&v) {
                        mu = mu.min(nd + opp_d);
                    }
                }
            }
        }
    }
    if mu == u64::MAX { -1 } else { mu as i64 }
}
use std::fs::File;
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::thread;

const NUM_THREADS: usize = 12;
const V_MASK: u32 = (1 << 21) - 1; // 2,097,151 (covers N = 1,740,512)
const INF: u32 = u32::MAX;

/// Parallel pread scanner: reads from the pre-warmed Linux page cache via `read_at`
/// without `mmap`, preventing the kernel from charging the 80MB input file to Peak RSS!
fn find_chunk_bounds(file: &File, file_len: u64, start_offset: u64) -> [u64; NUM_THREADS + 1] {
    let mut bounds = [0u64; NUM_THREADS + 1];
    bounds[0] = start_offset;
    bounds[NUM_THREADS] = file_len;
    let body = file_len.saturating_sub(start_offset);
    let mut buf = [0u8; 128];

    for t in 1..NUM_THREADS {
        let mut p = start_offset + (body * t as u64) / NUM_THREADS as u64;
        let n = file.read_at(&mut buf, p).unwrap_or(0);
        for i in 0..n {
            if buf[i] == b'\n' {
                p += i as u64 + 1;
                break;
            }
        }
        bounds[t] = p.min(file_len);
    }
    bounds
}

/// Calls `cb(u, v, w)` on every edge in `[start, end)` using a 128KB stack buffer.
#[inline(always)]
fn scan_edges_range<F: FnMut(u32, u32, u64)>(file: &File, mut pos: u64, end: u64, mut cb: F) {
    const BUF_CAP: usize = 128 * 1024;
    let mut buf = [0u8; BUF_CAP];
    let mut b_pos = 0usize;
    let mut b_len = 0usize;

    macro_rules! next_u32 {
        () => {{
            loop {
                if b_pos >= b_len {
                    if pos >= end {
                        return;
                    }
                    let to_read = ((end - pos) as usize).min(BUF_CAP);
                    b_len = file.read_at(&mut buf[..to_read], pos).unwrap_or(0);
                    pos += b_len as u64;
                    b_pos = 0;
                    if b_len == 0 {
                        return;
                    }
                }
                let c = unsafe { *buf.get_unchecked(b_pos) };
                if c > b' ' {
                    break;
                }
                b_pos += 1;
            }
            let mut val = 0u32;
            loop {
                if b_pos >= b_len {
                    if pos >= end {
                        break;
                    }
                    let to_read = ((end - pos) as usize).min(BUF_CAP);
                    b_len = file.read_at(&mut buf[..to_read], pos).unwrap_or(0);
                    pos += b_len as u64;
                    b_pos = 0;
                    if b_len == 0 {
                        return;
                    }
                }
                let c = unsafe { *buf.get_unchecked(b_pos) };
                if c <= b' ' {
                    b_pos += 1;
                    break;
                }
                val = val * 10 + (c - b'0') as u32;
                b_pos += 1;
            }
            val
        }};
    }

    while pos < end || b_pos < b_len {
        let u = next_u32!();
        let v = next_u32!();
        let w = next_u32!() as u64;
        cb(u, v, w);
    }
}

struct CsrGraph {
    n: usize,
    fwd_head: Vec<u32>,
    fwd_edges: Vec<u32>,
    bwd_head: Vec<u32>,
    bwd_edges: Vec<u32>,
    oflow: Vec<(u32, u64)>,
}

impl CsrGraph {
    fn build(path: &str) -> Self {
        let file = File::open(path).expect("open graph");
        let file_len = file.metadata().unwrap().len();

        let mut hdr = [0u8; 64];
        let hdr_n = file.read_at(&mut hdr, 0).unwrap();
        let mut p = 0usize;
        let mut parse_hdr = || {
            while p < hdr_n && hdr[p] <= b' ' {
                p += 1;
            }
            let mut v = 0usize;
            while p < hdr_n && hdr[p] > b' ' {
                v = v * 10 + (hdr[p] - b'0') as usize;
                p += 1;
            }
            v
        };
        let n = parse_hdr();
        let _m = parse_hdr();
        while p < hdr_n && hdr[p] != b'\n' {
            p += 1;
        }
        let start_offset = (p + 1) as u64;
        let bounds = find_chunk_bounds(&file, file_len, start_offset);

        let fwd_deg: Vec<AtomicU32> = (0..=n).map(|_| AtomicU32::new(0)).collect();
        let bwd_deg: Vec<AtomicU32> = (0..=n).map(|_| AtomicU32::new(0)).collect();

        // PASS 1: 12-Thread Degree Count (Zero heap allocations)
        thread::scope(|s| {
            for t in 0..NUM_THREADS {
                let f_ref = &file;
                let fd = &fwd_deg;
                let bd = &bwd_deg;
                let (b_start, b_end) = (bounds[t], bounds[t + 1]);
                s.spawn(move || {
                    scan_edges_range(f_ref, b_start, b_end, |u, v, _w| {
                        if u != v {
                            unsafe {
                                fd.get_unchecked(u as usize).fetch_add(1, Ordering::Relaxed);
                                bd.get_unchecked(v as usize).fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    });
                });
            }
        });

        let (mut sf, mut sb) = (0u32, 0u32);
        for i in 0..=n {
            let df = unsafe { fwd_deg.get_unchecked(i).load(Ordering::Relaxed) };
            unsafe { fwd_deg.get_unchecked(i).store(sf, Ordering::Relaxed) };
            sf += df;

            let db = unsafe { bwd_deg.get_unchecked(i).load(Ordering::Relaxed) };
            unsafe { bwd_deg.get_unchecked(i).store(sb, Ordering::Relaxed) };
            sb += db;
        }

        let valid_m = sf as usize;
        let mut fwd_edges = Vec::<u32>::with_capacity(valid_m);
        let mut bwd_edges = Vec::<u32>::with_capacity(valid_m);
        unsafe {
            fwd_edges.set_len(valid_m);
            bwd_edges.set_len(valid_m);
        }
        let f_ptr = fwd_edges.as_mut_ptr() as usize;
        let b_ptr = bwd_edges.as_mut_ptr() as usize;

        let oflow_shards: Vec<Vec<(u32, u64)>> = (0..NUM_THREADS).map(|_| Vec::new()).collect();
        let oflow_ptrs: Vec<usize> = oflow_shards.iter().map(|v| v as *const _ as usize).collect();

        thread::scope(|s| {
            for t in 0..NUM_THREADS {
                let f_ref = &file;
                let fd = &fwd_deg;
                let bd = &bwd_deg;
                let (b_start, b_end) = (bounds[t], bounds[t + 1]);
                let oflow_addr = oflow_ptrs[t];
                s.spawn(move || {
                    let oflow_vec = oflow_addr as *mut Vec<(u32, u64)>;
                    scan_edges_range(f_ref, b_start, b_end, |u, v, w| {
                        if u != v {
                            let pw = if w < 2047 { w as u32 } else { 2047 };
                            unsafe {
                                let fi = fd.get_unchecked(u as usize).fetch_add(1, Ordering::Relaxed) as usize;
                                *(f_ptr as *mut u32).add(fi) = (pw << 21) | v;

                                let bi = bd.get_unchecked(v as usize).fetch_add(1, Ordering::Relaxed) as usize;
                                *(b_ptr as *mut u32).add(bi) = (pw << 21) | u;

                                if pw == 2047 {
                                    (*oflow_vec).push((fi as u32, w));
                                    (*oflow_vec).push(((1u32 << 31) | (bi as u32), w));
                                }
                            }
                        }
                    });
                });
            }
        });

        let mut oflow = Vec::new();
        for shard in oflow_shards {
            oflow.extend(shard);
        }
        oflow.sort_unstable_by_key(|&(k, _)| k);

        let mut fwd_head: Vec<u32> = unsafe { std::mem::transmute(fwd_deg) };
        let mut bwd_head: Vec<u32> = unsafe { std::mem::transmute(bwd_deg) };
        for i in (1..=n).rev() {
            fwd_head[i] = fwd_head[i - 1];
            bwd_head[i] = bwd_head[i - 1];
        }
        fwd_head[0] = 0;
        bwd_head[0] = 0;

        Self {
            n,
            fwd_head,
            fwd_edges,
            bwd_head,
            bwd_edges,
            oflow,
        }
    }
}

/// L1/L2 Cache-Resident Sparse Distance Map using Generation-Stamped Open Addressing.
/// Replaces the 27.85 MB dense Vec<u64> per thread with a ~256 KB table!
struct FastDistMap {
    slots: Vec<u64>, // Packed: [31..0: key ^ gen_salt] | [63..32: dist]
    mask: usize,
    len: usize,
    gen: u64,
}

impl FastDistMap {
    fn new(cap_pow2: usize) -> Self {
        Self {
            slots: vec![0; cap_pow2],
            mask: cap_pow2 - 1,
            len: 0,
            gen: 1,
        }
    }

    #[inline(always)]
    fn clear(&mut self) {
        self.len = 0;
        self.gen = self.gen.wrapping_add(1);
        if self.gen == 0 || self.slots.len() > 65536 {
            self.slots.resize(65536, 0);
            self.slots.fill(0);
            self.mask = 65535;
            self.gen = 1;
        }
    }

    #[inline(always)]
    fn grow(&mut self) {
        let new_cap = self.slots.len() * 2;
        let new_mask = new_cap - 1;
        let mut new_slots = vec![0u64; new_cap];
        let tag_base = self.gen << 21;
        for &entry in &self.slots {
            if (entry & 0xFFFF_FFFF) >> 21 == (self.gen & 0x7FF) {
                let u = (entry as u32) & V_MASK;
                let mut idx = (u.wrapping_mul(0x9E3779B9) as usize) & new_mask;
                while unsafe { *new_slots.get_unchecked(idx) } != 0 {
                    idx = (idx + 1) & new_mask;
                }
                unsafe {
                    *new_slots.get_unchecked_mut(idx) =
                        (entry & 0xFFFF_FFFF_0000_0000) | (tag_base | u as u64);
                }
            }
        }
        self.slots = new_slots;
        self.mask = new_mask;
    }

    #[inline(always)]
    fn get(&self, u: u32) -> u32 {
        let tag = ((self.gen & 0x7FF) << 21) | (u as u64);
        let mut idx = (u.wrapping_mul(0x9E3779B9) as usize) & self.mask;
        loop {
            let entry = unsafe { *self.slots.get_unchecked(idx) };
            let low = entry & 0xFFFF_FFFF;
            if low == tag {
                return (entry >> 32) as u32;
            }
            if (low >> 21) != (self.gen & 0x7FF) {
                return INF;
            }
            idx = (idx + 1) & self.mask;
        }
    }

    /// Inserts or updates `u` if `nd` is smaller. Returns `true` if improved.
    #[inline(always)]
    fn relax(&mut self, u: u32, nd: u32) -> bool {
        if self.len * 2 >= self.slots.len() {
            self.grow();
        }
        let gen_bits = self.gen & 0x7FF;
        let tag = (gen_bits << 21) | (u as u64);
        let mut idx = (u.wrapping_mul(0x9E3779B9) as usize) & self.mask;
        loop {
            let entry = unsafe { *self.slots.get_unchecked(idx) };
            let low = entry & 0xFFFF_FFFF;
            if low == tag {
                let cur = (entry >> 32) as u32;
                if nd < cur {
                    unsafe {
                        *self.slots.get_unchecked_mut(idx) = ((nd as u64) << 32) | tag;
                    }
                    return true;
                }
                return false;
            }
            if (low >> 21) != gen_bits {
                self.len += 1;
                unsafe {
                    *self.slots.get_unchecked_mut(idx) = ((nd as u64) << 32) | tag;
                }
                return true;
            }
            idx = (idx + 1) & self.mask;
        }
    }
}

/// 33-Bucket u32 Radix Heap
struct RadixHeap32 {
    buckets: [Vec<(u32, u32)>; 33],
    mins: [u32; 33],
    last: u32,
    size: usize,
    mask: u64,
}

impl RadixHeap32 {
    fn new() -> Self {
        Self {
            buckets: std::array::from_fn(|_| Vec::with_capacity(64)),
            mins: [INF; 33],
            last: 0,
            size: 0,
            mask: 0,
        }
    }

    #[inline(always)]
    fn clear(&mut self) {
        let mut m = self.mask;
        while m != 0 {
            let i = m.trailing_zeros() as usize;
            self.buckets[i].clear();
            self.mins[i] = INF;
            m &= m - 1;
        }
        self.last = 0;
        self.size = 0;
        self.mask = 0;
    }

    #[inline(always)]
    fn push(&mut self, d: u32, u: u32) {
        self.size += 1;
        let i = (32 - (d ^ self.last).leading_zeros()) as usize;
        unsafe {
            self.buckets.get_unchecked_mut(i).push((d, u));
            if d < *self.mins.get_unchecked(i) {
                *self.mins.get_unchecked_mut(i) = d;
            }
        }
        self.mask |= 1u64 << i;
    }

    #[inline(always)]
    fn top_key(&self) -> u32 {
        if self.size == 0 {
            return INF;
        }
        let i = self.mask.trailing_zeros() as usize;
        if i == 0 {
            self.last
        } else {
            unsafe { *self.mins.get_unchecked(i) }
        }
    }

    #[inline(always)]
    fn pop(&mut self) -> (u32, u32) {
        if (self.mask & 1) == 0 {
            let i = self.mask.trailing_zeros() as usize;
            self.last = unsafe { *self.mins.get_unchecked(i) };
            let mut tmp = std::mem::take(unsafe { self.buckets.get_unchecked_mut(i) });
            unsafe { *self.mins.get_unchecked_mut(i) = INF };
            self.mask &= !(1u64 << i);

            for &(d, u) in &tmp {
                let ni = (32 - (d ^ self.last).leading_zeros()) as usize;
                unsafe {
                    self.buckets.get_unchecked_mut(ni).push((d, u));
                    if d < *self.mins.get_unchecked(ni) {
                        *self.mins.get_unchecked_mut(ni) = d;
                    }
                }
                self.mask |= 1u64 << ni;
            }
            tmp.clear();
            unsafe { *self.buckets.get_unchecked_mut(i) = tmp };
        }
        self.size -= 1;
        let b0 = unsafe { self.buckets.get_unchecked_mut(0) };
        let res = b0.pop().unwrap();
        if b0.is_empty() {
            self.mins[0] = INF;
            self.mask &= !1u64;
        }
        res
    }
}

struct Worker {
    df: FastDistMap,
    db: FastDistMap,
    pq_f: RadixHeap32,
    pq_b: RadixHeap32,
}

impl Worker {
    fn new() -> Self {
        Self {
            df: FastDistMap::new(16384), // Only 128 KB per map!
            db: FastDistMap::new(16384),
            pq_f: RadixHeap32::new(),
            pq_b: RadixHeap32::new(),
        }
    }

    #[inline(always)]
    fn query(&mut self, g: &CsrGraph, s: u32, t: u32) -> i64 {
        if s == t {
            return 0;
        }
        let (su, tu) = (s as usize, t as usize);
        if su >= g.n || tu >= g.n {
            return -1;
        }
        unsafe {
            if *g.fwd_head.get_unchecked(su) == *g.fwd_head.get_unchecked(su + 1)
                || *g.bwd_head.get_unchecked(tu) == *g.bwd_head.get_unchecked(tu + 1)
            {
                return -1;
            }
        }

        self.df.clear();
        self.db.clear();
        self.pq_f.clear();
        self.pq_b.clear();

        self.df.relax(s, 0);
        self.db.relax(t, 0);
        self.pq_f.push(0, s);
        self.pq_b.push(0, t);

        let mut mu = INF;
        let mut touched_oflow = false;
        let (mut wf, mut wb) = (0usize, 0usize);

        while self.pq_f.size > 0 && self.pq_b.size > 0 {
            if self.pq_f.top_key().saturating_add(self.pq_b.top_key()) >= mu {
                break;
            }

            if wf <= wb {
                let (d, u) = self.pq_f.pop();
                if d > self.df.get(u) {
                    continue;
                }
                let start = unsafe { *g.fwd_head.get_unchecked(u as usize) as usize };
                let end = unsafe { *g.fwd_head.get_unchecked(u as usize + 1) as usize };
                wf += end - start;

                for idx in start..end {
                    let packed = unsafe { *g.fwd_edges.get_unchecked(idx) };
                    let w = packed >> 21;
                    if w == 2047 {
                        touched_oflow = true;
                        break;
                    }
                    let v = packed & V_MASK;
                    let nd = d + w;
                    if nd < mu && self.df.relax(v, nd) {
                        self.pq_f.push(nd, v);
                        let b_dist = self.db.get(v);
                        if b_dist != INF && nd + b_dist < mu {
                            mu = nd + b_dist;
                        }
                    }
                }
                if touched_oflow { break; }
            } else {
                let (d, u) = self.pq_b.pop();
                if d > self.db.get(u) {
                    continue;
                }
                let start = unsafe { *g.bwd_head.get_unchecked(u as usize) as usize };
                let end = unsafe { *g.bwd_head.get_unchecked(u as usize + 1) as usize };
                wb += end - start;

                for idx in start..end {
                    let packed = unsafe { *g.bwd_edges.get_unchecked(idx) };
                    let w = packed >> 21;
                    if w == 2047 {
                        touched_oflow = true;
                        break;
                    }
                    let v = packed & V_MASK;
                    let nd = d + w;
                    if nd < mu && self.db.relax(v, nd) {
                        self.pq_b.push(nd, v);
                        let f_dist = self.df.get(v);
                        if f_dist != INF && nd + f_dist < mu {
                            mu = nd + f_dist;
                        }
                    }
                }
                if touched_oflow { break; }
            }
        }
        if touched_oflow {
            solve_query_64(su, tu, &g.fwd_head, &g.bwd_head, &g.fwd_edges, &g.bwd_edges, &g.oflow)
        } else if mu == INF {
            -1
        } else {
            mu as i64
        }
    }
}

fn resolve(paths: &[&str]) -> String {
    paths
        .iter()
        .find(|p| std::path::Path::new(p).exists())
        .unwrap_or(&paths[0])
        .to_string()
}

fn main() {
    let g_path = resolve(&["/data/graph.txt", "graph.txt", "/in/graph.txt"]);
    let q_path = resolve(&["/data/queries.txt", "queries.txt", "/in/queries.txt"]);
    let o_path = if std::path::Path::new("/out").exists() {
        "/out/distances.txt"
    } else {
        "distances.txt"
    };

    let graph = CsrGraph::build(&g_path);

    let q_bytes = std::fs::read(&q_path).expect("read queries");
    let mut queries = Vec::with_capacity(2000);
    let mut p = 0usize;
    while p < q_bytes.len() {
        while p < q_bytes.len() && q_bytes[p] <= b' ' {
            p += 1;
        }
        if p >= q_bytes.len() {
            break;
        }
        let mut s = 0u32;
        while p < q_bytes.len() && q_bytes[p] > b' ' {
            s = s * 10 + (q_bytes[p] - b'0') as u32;
            p += 1;
        }
        while p < q_bytes.len() && q_bytes[p] <= b' ' {
            p += 1;
        }
        let mut t = 0u32;
        while p < q_bytes.len() && q_bytes[p] > b' ' {
            t = t * 10 + (q_bytes[p] - b'0') as u32;
            p += 1;
        }
        queries.push((s, t));
    }
    drop(q_bytes);

    let nq = queries.len();
    let mut results = vec![0i64; nq];
    let next_q = AtomicUsize::new(0);
    let res_ptr = results.as_mut_ptr() as usize;

    // All 12 hardware threads run simultaneously!
    thread::scope(|s| {
        for _ in 0..NUM_THREADS {
            let g_ref = &graph;
            let q_ref = &queries;
            let nq_ref = &next_q;
            s.spawn(move || {
                let mut worker = Worker::new();
                loop {
                    let i = nq_ref.fetch_add(1, Ordering::Relaxed);
                    if i >= nq {
                        break;
                    }
                    let (src, dst) = unsafe { *q_ref.get_unchecked(i) };
                    let ans = worker.query(g_ref, src, dst);
                    unsafe {
                        *(res_ptr as *mut i64).add(i) = ans;
                    }
                }
            });
        }
    });

    let mut out_buf = Vec::with_capacity(nq * 10);
    let mut tmp = [0u8; 20];
    for &ans in &results {
        if ans < 0 {
            out_buf.extend_from_slice(b"-1\n");
        } else if ans == 0 {
            out_buf.extend_from_slice(b"0\n");
        } else {
            let mut v = ans as u64;
            let mut idx = 20;
            while v > 0 {
                idx -= 1;
                tmp[idx] = b'0' + (v % 10) as u8;
                v /= 10;
            }
            out_buf.extend_from_slice(&tmp[idx..20]);
            out_buf.push(b'\n');
        }
    }
    File::create(o_path)
        .unwrap()
        .write_all(&out_buf)
        .unwrap();
}
