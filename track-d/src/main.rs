use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering::Relaxed as R};
extern "C" {
    fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
    fn _exit(c: i32) -> !;
}
const E: u64 = u64::MAX;
const M: u64 = (1 << 21) - 1;
const NT: usize = 12;
const QT: usize = 6;

fn map(p: &str) -> &'static [u8] {
    let f = std::fs::File::open(p).unwrap();
    let l = f.metadata().unwrap().len() as usize;
    // MAP_SHARED | MAP_POPULATE: reuses host page cache, no heap charge
    unsafe { std::slice::from_raw_parts(mmap(std::ptr::null_mut(), l, 1, 0x8001, f.as_raw_fd(), 0), l) }
}
fn z<T>(k: usize) -> &'static [T] {
    let l = std::alloc::Layout::array::<T>(k.max(1)).unwrap();
    unsafe { Vec::from_raw_parts(std::alloc::alloc_zeroed(l) as *mut T, k, k.max(1)).leak() }
}
#[inline(always)]
fn num(b: &[u8], p: &mut usize) -> u64 {
    let mut x = 0;
    while b[*p] >= b'0' {
        x = x * 10 + (b[*p] - b'0') as u64;
        *p += 1;
    }
    *p += 1;
    x
}
fn each(b: &[u8], mut p: usize, hi: usize, mut f: impl FnMut(usize, usize, u32)) {
    while p < hi {
        let (u, v, w) = (num(b, &mut p) as usize, num(b, &mut p) as usize, num(b, &mut p) as u32);
        f(u, v, w);
    }
}
// open-addressing dist table: entry = dist << 21 | node; grows on demand, resets only touched slots
struct Tab { t: Vec<u64>, used: Vec<u32> }
impl Tab {
    fn slot(&self, k: u64) -> usize {
        let m = self.t.len() - 1;
        let mut i = (k.wrapping_mul(0x9E3779B97F4A7C15) >> 32) as usize & m;
        while self.t[i] != E && self.t[i] & M != k {
            i = (i + 1) & m;
        }
        i
    }
    fn get(&self, k: u64) -> u64 {
        let e = self.t[self.slot(k)];
        if e == E { E } else { e >> 21 }
    }
    fn set(&mut self, k: u64, d: u64) {
        if (self.used.len() + 1) * 2 > self.t.len() {
            let old: Vec<u64> = self.used.iter().map(|&i| self.t[i as usize]).collect();
            self.t = vec![E; self.t.len() * 2];
            self.used.clear();
            for e in old {
                let i = self.slot(e & M);
                self.used.push(i as u32);
                self.t[i] = e;
            }
        }
        let i = self.slot(k);
        if self.t[i] == E {
            self.used.push(i as u32);
        }
        self.t[i] = d << 21 | k;
    }
}
fn sp(h: &[AtomicU32], adj: &[AtomicU32], side: &[(u32, u32)], tb: &mut Tab, q: &mut BinaryHeap<Reverse<u64>>, s: usize, t: usize) -> i64 {
    for &i in &tb.used { tb.t[i as usize] = E; }
    tb.used.clear();
    q.clear();
    tb.set(s as u64, 0);
    q.push(Reverse(s as u64));
    while let Some(Reverse(x)) = q.pop() {
        let (u, d) = ((x & M) as usize, x >> 21);
        if u == t { return d as i64; }
        if tb.get(u as u64) < d { continue; }
        for i in h[u].load(R)..h[u + 1].load(R) {
            let e = adj[i as usize].load(R);
            let mut w = (e & 2047) as u64;
            if w == 2047 {
                w = side[side.binary_search_by_key(&i, |p| p.0).unwrap()].1 as u64;
            }
            let v = (e >> 11) as u64;
            if d + w < tb.get(v) {
                tb.set(v, d + w);
                q.push(Reverse((d + w) << 21 | v));
            }
        }
    }
    -1
}
fn main() {
    let g = map("/data/graph.txt");
    let mut p = 0;
    let n = num(g, &mut p) as usize;
    num(g, &mut p);
    let bd: &[usize] = (0..=NT)
        .map(|i| {
            if i == 0 { return p; }
            let mut x = p + (g.len() - p) * i / NT;
            while g[x - 1] != b'\n' { x += 1; }
            x
        })
        .collect::<Vec<_>>()
        .leak();
    let (head, inb) = (z::<AtomicU32>(n + 1), z::<AtomicU64>(n / 64 + 1));
    std::thread::scope(|s| {
        for i in 0..NT {
            s.spawn(move || {
                each(g, bd[i], bd[i + 1], |u, v, _| {
                    if u != v {
                        head[u].fetch_add(1, R);
                        inb[v >> 6].fetch_or(1 << (v & 63), R);
                    }
                })
            });
        }
    });
    let mut run = 0;
    for h in head.iter().take(n) {
        run += h.load(R);
        h.store(run, R);
    }
    head[n].store(run, R); // head[u] = exclusive end; pass 2 decrements it to the start
    let adj = z::<AtomicU32>(run as usize);
    let mut side: Vec<(u32, u32)> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..NT)
            .map(|i| {
                s.spawn(move || {
                    let mut sd = vec![];
                    each(g, bd[i], bd[i + 1], |u, v, w| {
                        if u != v {
                            let j = head[u].fetch_sub(1, R) - 1;
                            let c = w.min(2047); // 2047 = escape to side table
                            if c == 2047 { sd.push((j, w)); }
                            adj[j as usize].store((v as u32) << 11 | c, R);
                        }
                    });
                    sd
                })
            })
            .collect();
        hs.into_iter().flat_map(|h| h.join().unwrap()).collect()
    });
    side.sort_unstable();
    let side: &[(u32, u32)] = side.leak();
    let (qb, mut qs, mut p) = (std::fs::read("/data/queries.txt").unwrap(), vec![], 0);
    while p < qb.len() {
        qs.push((num(&qb, &mut p) as usize, num(&qb, &mut p) as usize));
    }
    let (qs, res, nxt): (&[(usize, usize)], _, _) = (qs.leak(), z::<AtomicI64>(qb.len() / 4 + 1), &AtomicUsize::new(0));
    std::thread::scope(|s| {
        for _ in 0..QT {
            s.spawn(move || {
                let (mut tb, mut q) = (Tab { t: vec![E; 4096], used: vec![] }, BinaryHeap::new());
                loop {
                    let i = nxt.fetch_add(1, R);
                    if i >= qs.len() { break; }
                    let (a, b) = qs[i];
                    let r = if a == b {
                        0
                    } else if head[a].load(R) == head[a + 1].load(R) || inb[b >> 6].load(R) >> (b & 63) & 1 == 0 {
                        -1
                    } else {
                        sp(head, adj, side, &mut tb, &mut q, a, b)
                    };
                    res[i].store(r, R);
                }
            });
        }
    });
    let mut o = String::new();
    for r in &res[..qs.len()] {
        o += &r.load(R).to_string();
        o.push('\n');
    }
    std::fs::write("/out/distances.txt", o).unwrap();
    unsafe { _exit(0) }
}
