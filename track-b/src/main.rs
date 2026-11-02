// speed_demon_b v2 — Track B (Index). Zero dependencies.
//
//  * queries first: only terms named by a query are ever looked at
//  * AVX2 64-byte block tokenizer (runtime-detected, scalar fallback)
//  * dense terms (by a tiny adaptive sample) -> 1 bit per doc, written
//    straight from the scan; rare terms -> sorted doc lists
//  * phrases are matched *during* the scan (bigram bloom + ring of the last
//    needed term ids) -> no positional index, per-thread top-10 accumulators
//  * BOOL chains evaluated on bitsets / lists, whichever is cheaper
//  * everything parallel over a work-stealing chunk list; chunk edges are
//    aligned to 64 doc ids so bitset words are never shared between threads
#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;
use std::env;
use std::ffi::CString;
use std::fs;
use std::os::raw::{c_int, c_void};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

/// Flip to true to madvise(DONTNEED) each scanned chunk (only helps if the
/// harness charges mapped page-cache pages to the container).
const RELEASE_PAGES: bool = false;
const K1: u64 = 0x9E37_79B9_7F4A_7C15;
const K2: u64 = 0xD6E8_FEB8_6659_FD93;

mod ffi {
    use super::*;
    extern "C" {
        pub fn open(path: *const i8, flags: c_int, ...) -> c_int;
        pub fn close(fd: c_int) -> c_int;
        pub fn mmap(a: *mut c_void, l: usize, p: c_int, f: c_int, fd: c_int, o: i64)
            -> *mut c_void;
        pub fn munmap(a: *mut c_void, l: usize) -> c_int;
        pub fn madvise(a: *mut c_void, l: usize, adv: c_int) -> c_int;
    }
}
struct Mmap {
    ptr: *mut u8,
    len: usize,
    fd: c_int,
}
unsafe impl Send for Mmap {}
unsafe impl Sync for Mmap {}
impl Mmap {
    fn open(path: &str) -> Mmap {
        let c = CString::new(path).unwrap();
        unsafe {
            let fd = ffi::open(c.as_ptr(), 0);
            assert!(fd >= 0, "open {}", path);
            let len = fs::metadata(path).expect("stat").len() as usize;
            if len == 0 {
                return Mmap {
                    ptr: std::ptr::null_mut(),
                    len: 0,
                    fd,
                };
            }
            let p = ffi::mmap(std::ptr::null_mut(), len, 1, 2, fd, 0);
            assert!(p as usize != usize::MAX, "mmap");
            Mmap {
                ptr: p as *mut u8,
                len,
                fd,
            }
        }
    }
    fn as_slice(&self) -> &[u8] {
        if self.len == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
        }
    }
    /// Pre-populate page tables for [start,start+len) in one syscall
    /// (MADV_POPULATE_READ = 22, Linux >= 5.14; errors ignored).
    fn populate(&self, start: usize, len: usize) {
        let page = 4096usize;
        let s = start & !(page - 1);
        unsafe {
            ffi::madvise(self.ptr.add(s) as *mut c_void, start + len - s, 22);
        }
    }
    fn release(&self, start: usize, len: usize) {
        let s = start & !4095usize;
        unsafe {
            ffi::madvise(self.ptr.add(s) as *mut c_void, start + len - s, 4);
        }
    }
}
impl Drop for Mmap {
    fn drop(&mut self) {
        unsafe {
            if self.len > 0 {
                ffi::munmap(self.ptr as *mut c_void, self.len);
            }
            ffi::close(self.fd);
        }
    }
}

// ---------------------------------------------------------------- tokens
static TOKTAB: [bool; 256] = {
    let mut t = [false; 256];
    let mut b = 0;
    while b < 256 {
        t[b] = (b >= 0x30 && b <= 0x39) || (b >= 0x41 && b <= 0x5A) || (b >= 0x61 && b <= 0x7A);
        b += 1;
    }
    t
};

/// Up to 8 token bytes as a lowered little-endian word. Token bytes are only
/// [A-Za-z0-9], for which `| 0x20` is exactly the spec's lowercasing.
#[inline(always)]
fn word_at(d: &[u8], i: usize, n: usize) -> u64 {
    let w = if i + 8 <= d.len() {
        u64::from_le(unsafe { (d.as_ptr().add(i) as *const u64).read_unaligned() })
    } else {
        let mut b = [0u8; 8];
        b[..n].copy_from_slice(&d[i..i + n]);
        u64::from_le_bytes(b)
    };
    let mask = if n >= 8 { !0u64 } else { (1u64 << (8 * n)) - 1 };
    (w | 0x2020_2020_2020_2020) & mask
}
#[inline(always)]
fn token_key(d: &[u8], s: usize, len: usize) -> u64 {
    if len <= 8 {
        return word_at(d, s, len);
    }
    let mut h = K1;
    let mut i = 0;
    while i + 8 <= len {
        h = (h.rotate_left(5) ^ word_at(d, s + i, 8)).wrapping_mul(K1);
        i += 8;
    }
    if i < len {
        h = (h.rotate_left(5) ^ word_at(d, s + i, len - i)).wrapping_mul(K1);
    }
    h ^ len as u64
}

/// A term that can ever equal a token: non-empty, only [a-z0-9].
fn valid_term(t: &[u8]) -> bool {
    !t.is_empty()
        && t.iter()
            .all(|&b| b.is_ascii_digit() || b.is_ascii_lowercase())
}
#[derive(Clone, Copy)]
struct Slot {
    key: u64,
    len: u32,
    id: i32,
}
struct Needed {
    slots: Vec<Slot>,
    mask: usize,
    shift: u32,
    bytes: Vec<Vec<u8>>,
    in_bool: Vec<bool>,
    in_phrase: Vec<bool>,
}
impl Needed {
    fn new(cap: usize) -> Needed {
        let mut n = Needed {
            slots: Vec::new(),
            mask: 0,
            shift: 0,
            bytes: Vec::new(),
            in_bool: Vec::new(),
            in_phrase: Vec::new(),
        };
        n.rebuild(cap);
        n
    }
    fn rebuild(&mut self, cap: usize) {
        let cap = cap.next_power_of_two().max(1024);
        self.slots = vec![
            Slot {
                key: 0,
                len: 0,
                id: -1
            };
            cap
        ];
        self.mask = cap - 1;
        self.shift = 64 - cap.trailing_zeros();
        for id in 0..self.bytes.len() {
            let t = &self.bytes[id];
            if !valid_term(t) {
                continue;
            }
            let key = token_key(t, 0, t.len());
            let mut i = (key.wrapping_mul(K2) >> self.shift) as usize;
            while self.slots[i].id >= 0 {
                i = (i + 1) & self.mask;
            }
            self.slots[i] = Slot {
                key,
                len: t.len() as u32,
                id: id as i32,
            };
        }
    }
    fn intern(&mut self, term: &[u8], phrase: bool) -> u32 {
        if (self.bytes.len() + 1) * 2 > self.slots.len() {
            let c = self.slots.len() * 2;
            self.rebuild(c);
        }
        if !valid_term(term) {
            // can never match a token (reference never matches it either): own id, no table slot
            let id = self.bytes.len() as u32;
            self.bytes.push(term.to_vec());
            self.in_bool.push(!phrase);
            self.in_phrase.push(phrase);
            return id;
        }
        let key = token_key(term, 0, term.len());
        let mut i = (key.wrapping_mul(K2) >> self.shift) as usize;
        loop {
            let s = self.slots[i];
            if s.id < 0 {
                let id = self.bytes.len() as u32;
                self.slots[i] = Slot {
                    key,
                    len: term.len() as u32,
                    id: id as i32,
                };
                self.bytes.push(term.to_vec());
                self.in_bool.push(!phrase);
                self.in_phrase.push(phrase);
                return id;
            }
            if s.key == key && s.len as usize == term.len() && self.bytes[s.id as usize] == term {
                if phrase {
                    self.in_phrase[s.id as usize] = true
                } else {
                    self.in_bool[s.id as usize] = true
                }
                return s.id as u32;
            }
            i = (i + 1) & self.mask;
        }
    }
    #[inline(always)]
    fn lookup(&self, d: &[u8], s: usize, len: usize) -> i32 {
        let key = token_key(d, s, len);
        let mut i = (key.wrapping_mul(K2) >> self.shift) as usize;
        loop {
            let sl = unsafe { *self.slots.get_unchecked(i) };
            if sl.id < 0 {
                return -1;
            }
            if sl.key == key && sl.len as usize == len {
                if len <= 8 {
                    return sl.id;
                }
                let tb = &self.bytes[sl.id as usize];
                if (0..len).all(|j| (d[s + j] | 0x20) == tb[j]) {
                    return sl.id;
                }
            }
            i = (i + 1) & self.mask;
        }
    }
}

// --------------------------------------------------------------- queries
#[derive(Clone, Copy, PartialEq)]
enum Op {
    And,
    Or,
    Not,
}
enum Query {
    Bool { first: u32, rest: Vec<(Op, u32)> },
    Phrase(u32),
}

fn parse_queries(raw: &[u8], nd: &mut Needed) -> (Vec<Query>, Vec<Vec<u32>>) {
    let mut qs = Vec::with_capacity(raw.len() / 24 + 1);
    let mut phrases: Vec<Vec<u32>> = Vec::new();
    for line in raw.split(|&b| b == b'\n') {
        let line = if line.last() == Some(&b'\r') {
            &line[..line.len() - 1]
        } else {
            line
        };
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix(b"BOOL ") {
            let mut w = rest.split(|&b| b == b' ');
            let first = nd.intern(w.next().unwrap(), false);
            let mut chain = Vec::new();
            while let Some(o) = w.next() {
                let op = match o {
                    b"AND" => Op::And,
                    b"OR" => Op::Or,
                    b"NOT" => Op::Not,
                    _ => panic!("op"),
                };
                chain.push((op, nd.intern(w.next().expect("term"), false)));
            }
            qs.push(Query::Bool { first, rest: chain });
        } else if let Some(rest) = line.strip_prefix(b"PHRASE ") {
            let inner = &rest[1..rest.len() - 1];
            let n_words = inner.split(|&b| b == b' ').count();
            if n_words == 1 {
                let t = nd.intern(inner, false);
                qs.push(Query::Bool {
                    first: t,
                    rest: Vec::new(),
                });
            } else {
                let terms: Vec<u32> = inner
                    .split(|&b| b == b' ')
                    .map(|w| nd.intern(w, true))
                    .collect();
                qs.push(Query::Phrase(phrases.len() as u32));
                phrases.push(terms);
            }
        } else {
            panic!("bad query");
        }
    }
    (qs, phrases)
}

// --------------------------------------------------------- phrase index
fn pair_h(a: u32, b: u32) -> u64 {
    (((a as u64) << 32) | b as u64).wrapping_mul(K1)
}
struct PhraseIdx {
    terms: Vec<Vec<u32>>,
    bloom: Vec<u64>,
    bloom_shift: u32,
    tkey: Vec<u64>,
    tstart: Vec<u32>,
    tlen: Vec<u32>,
    tmask: usize,
    tshift: u32,
    ids: Vec<u32>,
    ring_mask: usize,
}
impl PhraseIdx {
    fn build(terms: Vec<Vec<u32>>) -> PhraseIdx {
        let p = terms.len();
        let bbits = (((p * 16).max(1024)).next_power_of_two().trailing_zeros()).min(20);
        let mut bloom = vec![0u64; (1usize << bbits) / 64];
        let mut keyed: Vec<(u64, u32)> = Vec::with_capacity(p);
        let mut max_k = 2;
        for (pid, t) in terms.iter().enumerate() {
            let k = t.len();
            max_k = max_k.max(k);
            let h = pair_h(t[k - 2], t[k - 1]);
            let b = (h >> (64 - bbits)) as usize;
            bloom[b >> 6] |= 1 << (b & 63);
            keyed.push((((t[k - 2] as u64) << 32) | t[k - 1] as u64, pid as u32));
        }
        keyed.sort_unstable();
        let tcap = (p * 2).next_power_of_two().max(16);
        let tshift = 64 - tcap.trailing_zeros();
        let (mut tkey, mut tstart, mut tlen) =
            (vec![0u64; tcap], vec![0u32; tcap], vec![0u32; tcap]);
        let mut ids = Vec::with_capacity(p);
        let mut i = 0;
        while i < keyed.len() {
            let mut j = i;
            while j < keyed.len() && keyed[j].0 == keyed[i].0 {
                j += 1;
            }
            let key = keyed[i].0;
            let mut s = (key.wrapping_mul(K1) >> tshift) as usize;
            while tlen[s] != 0 {
                s = (s + 1) & (tcap - 1);
            }
            tkey[s] = key;
            tstart[s] = ids.len() as u32;
            tlen[s] = (j - i) as u32;
            for x in i..j {
                ids.push(keyed[x].1);
            }
            i = j;
        }
        PhraseIdx {
            terms,
            bloom,
            bloom_shift: 64 - bbits,
            tkey,
            tstart,
            tlen,
            tmask: tcap - 1,
            tshift,
            ids,
            ring_mask: max_k.next_power_of_two() - 1,
        }
    }
}
#[derive(Clone, Copy, Default)]
struct Acc {
    last: u32,
    cnt: u32,
    sum: u64,
    top: [u32; 10],
}

// ------------------------------------------------------------- scanning
struct DensePtr(*mut u64);
unsafe impl Send for DensePtr {}
unsafe impl Sync for DensePtr {}
struct Ctx<'a> {
    data: &'a [u8],
    needed: &'a Needed,
    dense_idx: &'a [u32],
    sparse: &'a [bool],
    dense: DensePtr,
    w: usize,
    n_docs: u32,
    ph: &'a PhraseIdx,
    has_ph: bool,
}
struct Worker {
    last_doc: Vec<u32>,
    hist: Vec<u32>,
    acc: Vec<Acc>,
}
struct St {
    doc: u32,
    run: usize,
    idx: usize,
}

#[inline(always)]
fn on_token(
    c: &Ctx,
    w: &mut Worker,
    st: &mut St,
    s: usize,
    len: usize,
    out: &mut Vec<(u32, u32)>,
    is_id: bool,
) {
    if is_id {
        let mut v = 0u32;
        for &b in &c.data[s..s + len] {
            v = v.wrapping_mul(10).wrapping_add((b - b'0') as u32);
        }
        assert!(v < c.n_docs, "doc id out of range");
        st.doc = v;
        st.run = 0;
        return;
    }
    let t = c.needed.lookup(c.data, s, len);
    if t < 0 {
        st.run = 0;
        return;
    }
    let t = t as usize;
    let di = unsafe { *c.dense_idx.get_unchecked(t) };
    if di != u32::MAX {
        unsafe {
            *c.dense.0.add(di as usize * c.w + (st.doc >> 6) as usize) |= 1u64 << (st.doc & 63);
        }
    } else if unsafe { *c.sparse.get_unchecked(t) } {
        if w.last_doc[t] != st.doc {
            w.last_doc[t] = st.doc;
            out.push((t as u32, st.doc));
        }
    }
    if c.has_ph {
        let ph = c.ph;
        let mask = ph.ring_mask;
        w.hist[st.idx & mask] = t as u32;
        st.idx = st.idx.wrapping_add(1);
        st.run += 1;
        if st.run >= 2 {
            let prev = w.hist[st.idx.wrapping_sub(2) & mask];
            let h = pair_h(prev, t as u32);
            let b = (h >> ph.bloom_shift) as usize;
            if (ph.bloom[b >> 6] >> (b & 63)) & 1 != 0 {
                let key = ((prev as u64) << 32) | t as u64;
                let mut sl = (key.wrapping_mul(K1) >> ph.tshift) as usize;
                while ph.tlen[sl] != 0 {
                    if ph.tkey[sl] == key {
                        let (a, n) = (ph.tstart[sl] as usize, ph.tlen[sl] as usize);
                        for &pid in &ph.ids[a..a + n] {
                            let terms = &ph.terms[pid as usize];
                            let k = terms.len();
                            if st.run < k {
                                continue;
                            }
                            let mut ok = true;
                            for j in 0..k - 2 {
                                if w.hist[st.idx.wrapping_sub(k - j) & mask] != terms[j] {
                                    ok = false;
                                    break;
                                }
                            }
                            if ok {
                                let ac = &mut w.acc[pid as usize];
                                if ac.last != st.doc + 1 {
                                    ac.last = st.doc + 1;
                                    if ac.cnt < 10 {
                                        ac.top[ac.cnt as usize] = st.doc;
                                    }
                                    ac.cnt += 1;
                                    ac.sum += st.doc as u64;
                                }
                            }
                        }
                        break;
                    }
                    sl = (sl + 1) & ph.tmask;
                }
            }
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn masks_avx2(p: *const u8) -> (u64, u64) {
    let (c20, c61, c19, c30, c9, cnl) = (
        _mm256_set1_epi8(0x20),
        _mm256_set1_epi8(0x61),
        _mm256_set1_epi8(25),
        _mm256_set1_epi8(0x30),
        _mm256_set1_epi8(9),
        _mm256_set1_epi8(b'\n' as i8),
    );
    let mut tok = 0u64;
    let mut nl = 0u64;
    for h in 0..2 {
        let v = _mm256_loadu_si256(p.add(h * 32) as *const __m256i);
        let a = _mm256_sub_epi8(_mm256_or_si256(v, c20), c61);
        let alpha = _mm256_cmpeq_epi8(_mm256_min_epu8(a, c19), a);
        let d = _mm256_sub_epi8(v, c30);
        let dig = _mm256_cmpeq_epi8(_mm256_min_epu8(d, c9), d);
        let t = _mm256_movemask_epi8(_mm256_or_si256(alpha, dig)) as u32 as u64;
        let n = _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, cnl)) as u32 as u64;
        tok |= t << (32 * h);
        nl |= n << (32 * h);
    }
    (tok, nl)
}
#[inline(always)]
unsafe fn masks_scalar(p: *const u8) -> (u64, u64) {
    let (mut tok, mut nl) = (0u64, 0u64);
    for j in 0..64 {
        let b = *p.add(j);
        tok |= (TOKTAB[b as usize] as u64) << j;
        nl |= ((b == b'\n') as u64) << j;
    }
    (tok, nl)
}

#[inline(always)]
unsafe fn scan_impl<const AVX: bool>(
    c: &Ctx,
    w: &mut Worker,
    cs: usize,
    ce: usize,
    out: &mut Vec<(u32, u32)>,
) {
    let n = c.data.len();
    let mut st = St {
        doc: 0,
        run: 0,
        idx: 0,
    };
    let (mut carry, mut nl_pending) = (0u64, true);
    let (mut pending, mut pend_id) = (usize::MAX, false);
    let mut tail;
    let mut i = cs;
    while i < ce {
        let remain = ce - i;
        let ptr = if i + 64 <= n {
            c.data.as_ptr().add(i)
        } else {
            tail = [0u8; 64];
            tail[..n - i].copy_from_slice(&c.data[i..]);
            tail.as_ptr()
        };
        #[cfg(target_arch = "x86_64")]
        let (mut tok, mut nl) = if AVX {
            masks_avx2(ptr)
        } else {
            masks_scalar(ptr)
        };
        #[cfg(not(target_arch = "x86_64"))]
        let (mut tok, mut nl) = masks_scalar(ptr);
        if remain < 64 {
            let v = (1u64 << remain) - 1;
            tok &= v;
            nl &= v;
        }
        let shifted = (tok << 1) | carry;
        let mut s_m = tok & !shifted;
        let mut e_m = !tok & shifted;
        carry = tok >> 63;
        let mut cur_nl = nl;
        loop {
            if pending != usize::MAX {
                if e_m == 0 {
                    break;
                }
                let e = e_m.trailing_zeros() as usize;
                e_m &= e_m - 1;
                on_token(c, w, &mut st, pending, i + e - pending, out, pend_id);
                pending = usize::MAX;
            } else {
                if s_m == 0 {
                    break;
                }
                let s = s_m.trailing_zeros() as usize;
                s_m &= s_m - 1;
                let low = (1u64 << s) - 1;
                if cur_nl & low != 0 {
                    nl_pending = true;
                }
                cur_nl &= !low;
                pend_id = nl_pending;
                nl_pending = false;
                pending = i + s;
            }
        }
        if cur_nl != 0 {
            nl_pending = true;
        }
        i += 64;
    }
    if pending != usize::MAX {
        on_token(c, w, &mut st, pending, ce - pending, out, pend_id);
    }
}
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,bmi1,bmi2,popcnt,lzcnt")]
unsafe fn scan_avx2(c: &Ctx, w: &mut Worker, cs: usize, ce: usize, out: &mut Vec<(u32, u32)>) {
    scan_impl::<true>(c, w, cs, ce, out)
}
unsafe fn scan_scalar(c: &Ctx, w: &mut Worker, cs: usize, ce: usize, out: &mut Vec<(u32, u32)>) {
    scan_impl::<false>(c, w, cs, ce, out)
}

fn has_avx2() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        return is_x86_feature_detected!("avx2")
            && is_x86_feature_detected!("bmi1")
            && is_x86_feature_detected!("bmi2")
            && is_x86_feature_detected!("popcnt")
            && is_x86_feature_detected!("lzcnt");
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

// --------------------------------------------------- planning / sampling
fn line_id(d: &[u8], at: usize) -> u32 {
    let mut v = 0u32;
    let mut i = at;
    while i < d.len() && d[i].is_ascii_digit() {
        v = v.wrapping_mul(10).wrapping_add((d[i] - b'0') as u32);
        i += 1;
    }
    v
}
fn next_line(d: &[u8], at: usize) -> usize {
    match d[at..].iter().position(|&b| b == b'\n') {
        Some(p) => at + p + 1,
        None => d.len(),
    }
}
/// Line-aligned chunk boundaries whose first doc id is a multiple of 64.
fn plan_chunks(d: &[u8], target: usize) -> Vec<(usize, usize)> {
    let n = d.len();
    if n == 0 {
        return Vec::new();
    }
    let mut b = vec![0usize];
    for k in 1..target {
        let x = n / target * k;
        if x <= *b.last().unwrap() {
            continue;
        }
        let mut p = if d[x - 1] == b'\n' {
            x
        } else {
            next_line(d, x)
        };
        while p < n && line_id(d, p) % 64 != 0 {
            p = next_line(d, p);
        }
        if p >= n {
            break;
        }
        if p > *b.last().unwrap() {
            b.push(p);
        }
    }
    b.push(n);
    b.windows(2).map(|w| (w[0], w[1])).collect()
}
/// Which needed BOOL terms are dense, from ~500 docs sampled across the file.
fn sample_dense(d: &[u8], nd: &Needed, div: u64) -> Vec<bool> {
    let m = nd.bytes.len();
    let (mut df, mut stamp) = (vec![0u64; m], vec![0u32; m]);
    let n = d.len();
    let regions: Vec<(usize, usize)> = if n <= 512 * 1024 {
        vec![(0, n)]
    } else {
        (0..4)
            .map(|j| {
                let s = n / 4 * j;
                (s, s + 128 * 1024)
            })
            .collect()
    };
    let (mut docs, mut line_no) = (0u64, 0u32);
    for (rs, re) in regions {
        let mut p = if rs == 0 || d[rs - 1] == b'\n' {
            rs
        } else {
            next_line(d, rs)
        };
        while p < re && p < n {
            let e = {
                let nx = next_line(d, p);
                if nx > p && d[nx - 1] == b'\n' {
                    nx - 1
                } else {
                    nx
                }
            };
            let line_end = e;
            if let Some(tab) = d[p..line_end].iter().position(|&b| b == b'\t') {
                line_no += 1;
                docs += 1;
                let mut i = p + tab + 1;
                while i < line_end {
                    if !TOKTAB[d[i] as usize] {
                        i += 1;
                        continue;
                    }
                    let s = i;
                    while i < line_end && TOKTAB[d[i] as usize] {
                        i += 1;
                    }
                    let t = nd.lookup(d, s, i - s);
                    if t >= 0 && stamp[t as usize] != line_no {
                        stamp[t as usize] = line_no;
                        df[t as usize] += 1;
                    }
                }
            }
            p = next_line(d, p);
        }
    }
    (0..m)
        .map(|t| nd.in_bool[t] && df[t] > 0 && df[t] * div >= docs)
        .collect()
}

// ----------------------------------------------------------- evaluation
#[derive(Clone, Copy, Default)]
struct Summary {
    count: u64,
    sum: u64,
    first10: [u32; 10],
    n10: usize,
}
fn summarize(ids: &[u32]) -> Summary {
    let mut s = Summary {
        count: ids.len() as u64,
        n10: ids.len().min(10),
        ..Default::default()
    };
    for (k, &id) in ids.iter().enumerate() {
        s.sum += id as u64;
        if k < 10 {
            s.first10[k] = id;
        }
    }
    s
}
#[inline(always)]
fn summarize_bits(bits: &[u64]) -> Summary {
    const M: [u64; 6] = [
        0xAAAA_AAAA_AAAA_AAAA,
        0xCCCC_CCCC_CCCC_CCCC,
        0xF0F0_F0F0_F0F0_F0F0,
        0xFF00_FF00_FF00_FF00,
        0xFFFF_0000_FFFF_0000,
        0xFFFF_FFFF_0000_0000,
    ];
    let mut s = Summary::default();
    for (wi, &w) in bits.iter().enumerate() {
        if w == 0 {
            continue;
        }
        let c = w.count_ones() as u64;
        let base = wi as u64 * 64;
        let mut ps = 0u64;
        for k in 0..6 {
            ps += ((w & M[k]).count_ones() as u64) << k;
        }
        s.count += c;
        s.sum += c * base + ps;
        if s.n10 < 10 {
            let mut x = w;
            while x != 0 && s.n10 < 10 {
                s.first10[s.n10] = (base + x.trailing_zeros() as u64) as u32;
                s.n10 += 1;
                x &= x - 1;
            }
        }
    }
    s
}
fn log2_ceil(n: usize) -> u64 {
    if n <= 1 {
        0
    } else {
        (usize::BITS - (n - 1).leading_zeros()) as u64
    }
}
fn merge_and(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    out.clear();
    if a.is_empty() || b.is_empty() {
        return;
    }
    let (small, large) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    if (small.len() as u64) * log2_ceil(large.len()) < (a.len() + b.len()) as u64 {
        for &v in small {
            if large.binary_search(&v).is_ok() {
                out.push(v);
            }
        }
        return;
    }
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            out.push(a[i]);
            i += 1;
            j += 1;
        } else if a[i] < b[j] {
            i += 1
        } else {
            j += 1
        }
    }
}
fn merge_or(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    out.clear();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            out.push(a[i]);
            i += 1;
            j += 1;
        } else if a[i] < b[j] {
            out.push(a[i]);
            i += 1;
        } else {
            out.push(b[j]);
            j += 1;
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
}
fn merge_not(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    out.clear();
    if a.is_empty() {
        return;
    }
    if b.is_empty() {
        out.extend_from_slice(a);
        return;
    }
    if (a.len() as u64) * log2_ceil(b.len()) < (a.len() + b.len()) as u64 {
        for &v in a {
            if b.binary_search(&v).is_err() {
                out.push(v);
            }
        }
        return;
    }
    let (mut i, mut j) = (0, 0);
    while i < a.len() {
        if j < b.len() && b[j] < a[i] {
            j += 1
        } else if j < b.len() && b[j] == a[i] {
            i += 1;
            j += 1
        } else {
            out.push(a[i]);
            i += 1
        }
    }
}
struct EvalCtx<'a> {
    dense: &'a [u64],
    w: usize,
    dense_idx: &'a [u32],
    sp_start: &'a [u32],
    sp_docs: &'a [u32],
}
struct Scratch {
    la: Vec<u32>,
    lb: Vec<u32>,
    bits: Vec<u64>,
}
enum Opd<'a> {
    Dense(&'a [u64]),
    Sparse(&'a [u32]),
}
#[inline(always)]
fn bit(b: &[u64], x: u32) -> bool {
    (b[(x >> 6) as usize] >> (x & 63)) & 1 != 0
}

#[inline(always)]
fn eval_impl(first: u32, rest: &[(Op, u32)], ev: &EvalCtx, sc: &mut Scratch) -> Summary {
    let opd = |t: u32| -> Opd {
        let di = ev.dense_idx[t as usize];
        if di != u32::MAX {
            Opd::Dense(&ev.dense[di as usize * ev.w..(di as usize + 1) * ev.w])
        } else {
            Opd::Sparse(
                &ev.sp_docs[ev.sp_start[t as usize] as usize..ev.sp_start[t as usize + 1] as usize],
            )
        }
    };
    let mut bits_mode;
    match opd(first) {
        Opd::Dense(d) => {
            sc.bits.copy_from_slice(d);
            bits_mode = true;
        }
        Opd::Sparse(l) => {
            sc.la.clear();
            sc.la.extend_from_slice(l);
            bits_mode = false;
        }
    }
    for &(op, t) in rest {
        match (bits_mode, op, opd(t)) {
            (false, Op::And, Opd::Sparse(l)) => {
                merge_and(&sc.la, l, &mut sc.lb);
                std::mem::swap(&mut sc.la, &mut sc.lb);
            }
            (false, Op::Or, Opd::Sparse(l)) => {
                merge_or(&sc.la, l, &mut sc.lb);
                std::mem::swap(&mut sc.la, &mut sc.lb);
            }
            (false, Op::Not, Opd::Sparse(l)) => {
                merge_not(&sc.la, l, &mut sc.lb);
                std::mem::swap(&mut sc.la, &mut sc.lb);
            }
            (false, Op::And, Opd::Dense(d)) => {
                sc.lb.clear();
                for &x in &sc.la {
                    if bit(d, x) {
                        sc.lb.push(x);
                    }
                }
                std::mem::swap(&mut sc.la, &mut sc.lb);
            }
            (false, Op::Not, Opd::Dense(d)) => {
                sc.lb.clear();
                for &x in &sc.la {
                    if !bit(d, x) {
                        sc.lb.push(x);
                    }
                }
                std::mem::swap(&mut sc.la, &mut sc.lb);
            }
            (false, Op::Or, Opd::Dense(d)) => {
                sc.bits.copy_from_slice(d);
                for &x in &sc.la {
                    sc.bits[(x >> 6) as usize] |= 1u64 << (x & 63);
                }
                bits_mode = true;
            }
            (true, Op::And, Opd::Dense(d)) => {
                for (a, b) in sc.bits.iter_mut().zip(d) {
                    *a &= *b;
                }
            }
            (true, Op::Or, Opd::Dense(d)) => {
                for (a, b) in sc.bits.iter_mut().zip(d) {
                    *a |= *b;
                }
            }
            (true, Op::Not, Opd::Dense(d)) => {
                for (a, b) in sc.bits.iter_mut().zip(d) {
                    *a &= !*b;
                }
            }
            (true, Op::And, Opd::Sparse(l)) => {
                sc.lb.clear();
                for &x in l {
                    if bit(&sc.bits, x) {
                        sc.lb.push(x);
                    }
                }
                std::mem::swap(&mut sc.la, &mut sc.lb);
                bits_mode = false;
            }
            (true, Op::Or, Opd::Sparse(l)) => {
                for &x in l {
                    sc.bits[(x >> 6) as usize] |= 1u64 << (x & 63);
                }
            }
            (true, Op::Not, Opd::Sparse(l)) => {
                for &x in l {
                    sc.bits[(x >> 6) as usize] &= !(1u64 << (x & 63));
                }
            }
        }
    }
    if bits_mode {
        summarize_bits(&sc.bits)
    } else {
        summarize(&sc.la)
    }
}
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,bmi1,bmi2,popcnt,lzcnt")]
unsafe fn eval_avx2(first: u32, rest: &[(Op, u32)], ev: &EvalCtx, sc: &mut Scratch) -> Summary {
    eval_impl(first, rest, ev, sc)
}

// ------------------------------------------------------------------ io
fn push_u64(buf: &mut Vec<u8>, mut v: u64) {
    if v == 0 {
        buf.push(b'0');
        return;
    }
    let mut t = [0u8; 20];
    let mut i = 20;
    while v > 0 {
        i -= 1;
        t[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    buf.extend_from_slice(&t[i..]);
}
fn write_summary(buf: &mut Vec<u8>, s: &Summary) {
    push_u64(buf, s.count);
    buf.push(b' ');
    push_u64(buf, s.sum);
    for i in 0..s.n10 {
        buf.push(b' ');
        push_u64(buf, s.first10[i] as u64);
    }
    buf.push(b'\n');
}
fn env_usize(k: &str) -> Option<usize> {
    env::var(k).ok().and_then(|v| v.parse().ok())
}

fn main() {
    let t0 = Instant::now();
    let args: Vec<String> = env::args().collect();
    let (mut data_dir, mut out_dir) = ("/data".to_string(), "/out".to_string());
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--data" if i + 1 < args.len() => {
                data_dir = args[i + 1].clone();
                i += 2;
            }
            "--out" if i + 1 < args.len() => {
                out_dir = args[i + 1].clone();
                i += 2;
            }
            _ => i += 1,
        }
    }
    let avx = has_avx2();
    let n_threads = env_usize("SPEED_DEMON_THREADS")
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|p| p.get())
                .unwrap_or(4)
                .min(12)
        })
        .max(1);

    let qraw = fs::read(format!("{}/queries.txt", data_dir)).expect("queries.txt");
    let mut needed = Needed::new(1 << 15);
    let (queries, phrases) = parse_queries(&qraw, &mut needed);
    drop(qraw);
    let m = needed.bytes.len();
    let cap = m * 4;
    needed.rebuild(cap);
    let n_phr = phrases.len();
    let ph = PhraseIdx::build(phrases);
    let has_ph = n_phr > 0;

    let mmap = Mmap::open(&format!("{}/docs.txt", data_dir));
    let data = mmap.as_slice();
    let n = data.len();
    // N = last id + 1 (ids are dense 0..N-1)
    let n_docs: u32 = if n == 0 {
        0
    } else {
        let mut e = n;
        while e > 0 && data[e - 1] == b'\n' {
            e -= 1;
        }
        let mut s = e;
        while s > 0 && data[s - 1] != b'\n' {
            s -= 1;
        }
        line_id(data, s) + 1
    };
    let w = ((n_docs as usize) + 63) / 64;

    let div = env_usize("SPEED_DEMON_DENSE_DIV").unwrap_or(64) as u64;
    let dense_flag = sample_dense(data, &needed, div);
    let mut dense_idx = vec![u32::MAX; m];
    let mut n_dense = 0usize;
    for t in 0..m {
        if dense_flag[t] {
            dense_idx[t] = n_dense as u32;
            n_dense += 1;
        }
    }
    let sparse: Vec<bool> = (0..m)
        .map(|t| needed.in_bool[t] && !dense_flag[t])
        .collect();
    let mut dense_bits = vec![0u64; n_dense * w];

    let target = env_usize("SPEED_DEMON_CHUNKS")
        .unwrap_or_else(|| (n / (256 * 1024)).clamp(1, n_threads * 24));
    let plans = plan_chunks(data, target);
    let ctx = Ctx {
        data,
        needed: &needed,
        dense_idx: &dense_idx,
        sparse: &sparse,
        dense: DensePtr(dense_bits.as_mut_ptr()),
        w,
        n_docs,
        ph: &ph,
        has_ph,
    };
    let next = AtomicUsize::new(0);
    let results: Vec<Mutex<Vec<(u32, u32)>>> =
        (0..plans.len()).map(|_| Mutex::new(Vec::new())).collect();
    let accs: Mutex<Vec<Vec<Acc>>> = Mutex::new(Vec::new());
    std::thread::scope(|sc| {
        for _ in 0..n_threads.min(plans.len().max(1)) {
            let (ctx, plans, next, results, accs, mmap) =
                (&ctx, &plans, &next, &results, &accs, &mmap);
            sc.spawn(move || {
                let mut wk = Worker {
                    last_doc: vec![u32::MAX; m],
                    hist: vec![0; ctx.ph.ring_mask + 1],
                    acc: vec![Acc::default(); n_phr],
                };
                loop {
                    let k = next.fetch_add(1, Ordering::Relaxed);
                    if k >= plans.len() {
                        break;
                    }
                    let (cs, ce) = plans[k];
                    mmap.populate(cs, ce - cs);
                    let mut out = Vec::new();
                    unsafe {
                        #[cfg(target_arch = "x86_64")]
                        if avx {
                            scan_avx2(ctx, &mut wk, cs, ce, &mut out)
                        } else {
                            scan_scalar(ctx, &mut wk, cs, ce, &mut out)
                        }
                        #[cfg(not(target_arch = "x86_64"))]
                        scan_scalar(ctx, &mut wk, cs, ce, &mut out);
                    }
                    if RELEASE_PAGES {
                        mmap.release(cs, ce - cs);
                    }
                    *results[k].lock().unwrap() = out;
                }
                accs.lock().unwrap().push(wk.acc);
            });
        }
    });
    drop(mmap);

    // sparse lists: counting sort over chunks taken in file order (already doc-sorted)
    let bufs: Vec<Vec<(u32, u32)>> = results
        .into_iter()
        .map(|x| x.into_inner().unwrap())
        .collect();
    let mut sp_start = vec![0u32; m + 1];
    for b in &bufs {
        for &(t, _) in b {
            sp_start[t as usize + 1] += 1;
        }
    }
    for t in 0..m {
        sp_start[t + 1] += sp_start[t];
    }
    let mut sp_docs = vec![0u32; sp_start[m] as usize];
    let mut cur = sp_start.clone();
    for b in &bufs {
        for &(t, d) in b {
            sp_docs[cur[t as usize] as usize] = d;
            cur[t as usize] += 1;
        }
    }
    drop(bufs);
    drop(cur);

    // phrase accumulators
    let accs = accs.into_inner().unwrap();
    let mut phrase_sum: Vec<Summary> = Vec::with_capacity(n_phr);
    for pid in 0..n_phr {
        let mut s = Summary::default();
        let mut tops: Vec<u32> = Vec::new();
        for a in &accs {
            let x = &a[pid];
            s.count += x.cnt as u64;
            s.sum += x.sum;
            tops.extend_from_slice(&x.top[..(x.cnt as usize).min(10)]);
        }
        tops.sort_unstable();
        s.n10 = tops.len().min(10);
        s.first10[..s.n10].copy_from_slice(&tops[..s.n10]);
        phrase_sum.push(s);
    }
    drop(accs);

    // BOOL evaluation, parallel; phrase answers are looked up
    let ev = EvalCtx {
        dense: &dense_bits,
        w,
        dense_idx: &dense_idx,
        sp_start: &sp_start,
        sp_docs: &sp_docs,
    };
    let mut res: Vec<Summary> = vec![Summary::default(); queries.len()];
    if !queries.is_empty() {
        let chunk_len = (queries.len() + n_threads - 1) / n_threads;
        std::thread::scope(|sc| {
            let (mut rq, mut rr): (&[Query], &mut [Summary]) = (&queries, &mut res);
            while !rq.is_empty() {
                let take = chunk_len.min(rq.len());
                let (qc, qr) = rq.split_at(take);
                let (rc, rrest) = rr.split_at_mut(take);
                rq = qr;
                rr = rrest;
                let (ev, phrase_sum) = (&ev, &phrase_sum);
                sc.spawn(move || {
                    let mut s = Scratch {
                        la: Vec::new(),
                        lb: Vec::new(),
                        bits: vec![0u64; ev.w],
                    };
                    for (q, r) in qc.iter().zip(rc.iter_mut()) {
                        *r = match q {
                            Query::Phrase(pid) => phrase_sum[*pid as usize],
                            Query::Bool { first, rest } => unsafe {
                                #[cfg(target_arch = "x86_64")]
                                if avx {
                                    eval_avx2(*first, rest, ev, &mut s)
                                } else {
                                    eval_impl(*first, rest, ev, &mut s)
                                }
                                #[cfg(not(target_arch = "x86_64"))]
                                eval_impl(*first, rest, ev, &mut s)
                            },
                        };
                    }
                });
            }
        });
    }
    let mut out = Vec::with_capacity(queries.len() * 24);
    for s in &res {
        write_summary(&mut out, s);
    }
    fs::create_dir_all(&out_dir).expect("mkdir");
    fs::write(format!("{}/results.txt", out_dir), &out).expect("write");
    if env::var("SPEED_DEMON_DEBUG").is_ok() {
        eprintln!(
            "terms={} dense={} phrases={} chunks={} threads={} avx2={} n_docs={} elapsed={:?}",
            m,
            n_dense,
            n_phr,
            plans.len(),
            n_threads,
            avx,
            n_docs,
            t0.elapsed()
        );
    }
}
