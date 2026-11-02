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

fn map(p: &str) -> &'static [u8] {
    let f = std::fs::File::open(p).unwrap();
    let l = f.metadata().unwrap().len() as usize;
    unsafe { std::slice::from_raw_parts(mmap(std::ptr::null_mut(), l, 1, 0x8001, f.as_raw_fd(), 0), l) }
}
fn z<T>(k: usize) -> &'static [T] {
    let l = std::alloc::Layout::array::<T>(k.max(1)).unwrap();
    unsafe { Vec::from_raw_parts(std::alloc::alloc_zeroed(l) as *mut T, k, k.max(1)).leak() }
}
#[inline(always)]
fn num(b: &[u8], p: &mut usize) -> u64 {
    let mut x = 0;
    unsafe {
        while *b.get_unchecked(*p) >= b'0' {
            x = x * 10 + (*b.get_unchecked(*p) - b'0') as u64;
            *p += 1;
        }
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
// Per-thread Dijkstra state: dist stored in lazily allocated 32-node pages, O(1) indexing, O(touched) reset.
struct Ws { pi: Vec<u32>, pg: Vec<[u64; 32]>, dirty: Vec<u32>, h: BinaryHeap<Reverse<u64>> }
impl Ws {
    fn at(&mut self, v: usize) -> &mut u64 {
        let p = &mut self.pi[v >> 5];
        if *p == u32::MAX {
            *p = self.dirty.len() as u32;
            self.dirty.push((v >> 5) as u32);
            match self.pg.get_mut(*p as usize) {
                Some(pg) => *pg = [E; 32],
                None => self.pg.push([E; 32]),
            }
        }
        &mut self.pg[*p as usize][v & 31]
    }
}
fn sp(w: &mut Ws, h: &[AtomicU32], adj: &[AtomicU32], side: &[(u32, u32)], s: usize, t: usize) -> i64 {
    for &p in &w.dirty {
        w.pi[p as usize] = u32::MAX;
    }
    (w.dirty.clear(), w.h.clear());
    w.pg.truncate(4096);
    w.pg.shrink_to(4096);
    *w.at(s) = 0;
    w.h.push(Reverse(s as u64));
    while let Some(Reverse(x)) = w.h.pop() {
        let (u, d) = ((x & M) as usize, x >> 21);
        if u == t {
            return d as i64;
        }
        if d > *w.at(u) {
            continue;
        }
        for i in h[u].load(R)..h[u + 1].load(R) {
            let e = adj[i as usize].load(R);
            let mut c = (e & 2047) as u64;
            if c == 2047 {
                c = side[side.binary_search_by_key(&i, |p| p.0).unwrap()].1 as u64;
            }
            let v = (e >> 11) as usize;
            let a = w.at(v);
            if d + c < *a {
                *a = d + c;
                w.h.push(Reverse((d + c) << 21 | v as u64));
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
            let mut x = p + (g.len() - p) * i / NT;
            while i > 0 && g[x - 1] != b'\n' {
                x += 1;
            }
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
    head[n].store(run, R); // head[u] = end offset; pass 2 decrements it down to the start offset
    let adj = z::<AtomicU32>(run as usize);
    let mut side: Vec<(u32, u32)> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..NT)
            .map(|i| {
                s.spawn(move || {
                    let mut sd = vec![];
                    each(g, bd[i], bd[i + 1], |u, v, w| {
                        if u != v {
                            let j = head[u].fetch_sub(1, R) - 1;
                            let c = w.min(2047); // 2047 = escape into side table
                            if c == 2047 {
                                sd.push((j, w));
                            }
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
    let (qb, mut qs, mut p) = (map("/data/queries.txt"), vec![], 0);
    while p < qb.len() {
        qs.push((num(qb, &mut p) as usize, num(qb, &mut p) as usize));
    }
    let (qs, res, nxt): (&[(usize, usize)], _, _) = (qs.leak(), z::<AtomicI64>(qb.len() / 4 + 1), &AtomicUsize::new(0));
    std::thread::scope(|s| {
        for _ in 0..NT {
            s.spawn(move || {
                let mut w = Ws { pi: vec![u32::MAX; (n >> 5) + 1], pg: vec![], dirty: vec![], h: BinaryHeap::new() };
                loop {
                    let i = nxt.fetch_add(1, R);
                    if i >= qs.len() {
                        break;
                    }
                    let (a, b) = qs[i];
                    res[i].store(
                        if a == b {
                            0
                        } else if head[a].load(R) == head[a + 1].load(R) || inb[b >> 6].load(R) >> (b & 63) & 1 == 0 {
                            -1
                        } else {
                            sp(&mut w, head, adj, side, a, b)
                        },
                        R,
                    );
                }
            });
        }
    });
    let mut o = String::new();
    for r in &res[..qs.len()] {
        o += &format!("{}\n", r.load(R));
    }
    std::fs::write("/out/distances.txt", o).unwrap();
    unsafe { _exit(0) }
}
