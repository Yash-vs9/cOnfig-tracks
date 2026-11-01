// speed_demon_b — Track B (Index), no external dependencies.
//
// Design (why it's shaped this way is in the chat message, not here):
//   1. Parse queries.txt FIRST. Only terms that actually appear in a query
//      are ever interned or indexed ("query-driven index"). Corpus
//      vocabulary not referenced by any query costs nothing.
//   2. mmap docs.txt read-only. Split into many small line-aligned chunks
//      (far more chunks than threads) and hand them out from a shared
//      atomic counter (work-stealing) across a fixed pool of threads sized
//      to the pinned core count.
//   3. Each worker tokenizes its chunk with a 256-entry branch-light
//      lookup table, hashes each token while lowering it (no intermediate
//      string allocation for the common "not a needed term" case), and
//      only records an occurrence when the token hashes into the small
//      needed-term table. Position counters still advance for skipped
//      tokens, since phrase matching depends on them.
//   4. As soon as a chunk is fully scanned, its mmap pages are released
//      with madvise(MADV_DONTNEED) so resident memory never approaches
//      the 254 MB corpus size — only a handful of in-flight chunks are
//      resident at once.
//   5. Per-term occurrence lists are assembled with a linear counting
//      sort (bucket by term id) over chunks taken in file order, so the
//      result is already sorted per term with no comparison sort needed.
//   6. Queries are answered with sorted-array merges (BOOL) or a
//      shortest-list-driven positional binary search (PHRASE), using
//      double-buffered scratch space so the whole 50k-query pass does
//      no per-query heap allocation.

use std::env;
use std::ffi::CString;
use std::fs;
use std::os::raw::{c_int, c_void};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

// ---------------------------------------------------------------------
// Minimal libc FFI (no `libc` crate dependency — these are the only
// system calls we need, declared by hand against the system libc that
// std itself already links).
// ---------------------------------------------------------------------
mod ffi {
    use super::*;
    extern "C" {
        pub fn open(path: *const i8, flags: c_int, ...) -> c_int;
        pub fn close(fd: c_int) -> c_int;
        pub fn mmap(
            addr: *mut c_void,
            len: usize,
            prot: c_int,
            flags: c_int,
            fd: c_int,
            offset: i64,
        ) -> *mut c_void;
        pub fn munmap(addr: *mut c_void, len: usize) -> c_int;
        pub fn madvise(addr: *mut c_void, len: usize, advice: c_int) -> c_int;
    }
    pub const O_RDONLY: c_int = 0;
    pub const PROT_READ: c_int = 1;
    pub const MAP_PRIVATE: c_int = 0x02;
    pub const MAP_FAILED: *mut c_void = usize::MAX as *mut c_void;
    pub const MADV_SEQUENTIAL: c_int = 2;
    pub const MADV_WILLNEED: c_int = 3;
    pub const MADV_DONTNEED: c_int = 4;
}

/// A read-only mmap of a whole file, released on drop.
struct Mmap {
    ptr: *mut u8,
    len: usize,
    fd: c_int,
}
unsafe impl Send for Mmap {}
unsafe impl Sync for Mmap {}
impl Mmap {
    fn open(path: &str) -> Mmap {
        let c_path = CString::new(path).expect("path has no NUL bytes");
        unsafe {
            let fd = ffi::open(c_path.as_ptr(), ffi::O_RDONLY);
            assert!(fd >= 0, "open({}) failed", path);
            let len = fs::metadata(path)
                .unwrap_or_else(|_| panic!("stat({}) failed", path))
                .len() as usize;
            if len == 0 {
                return Mmap { ptr: std::ptr::null_mut(), len: 0, fd };
            }
            let ptr = ffi::mmap(
                std::ptr::null_mut(),
                len,
                ffi::PROT_READ,
                ffi::MAP_PRIVATE,
                fd,
                0,
            );
            assert!(ptr != ffi::MAP_FAILED, "mmap({}) failed", path);
            ffi::madvise(ptr as *mut c_void, len, ffi::MADV_SEQUENTIAL);
            Mmap { ptr: ptr as *mut u8, len, fd }
        }
    }
    #[inline]
    fn as_slice(&self) -> &[u8] {
        if self.len == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
        }
    }
    /// Readahead-hint a byte range about to be scanned.
    #[inline]
    fn willneed(&self, start: usize, len: usize) {
        unsafe {
            ffi::madvise(self.ptr.add(start) as *mut c_void, len, ffi::MADV_WILLNEED);
        }
    }
    /// Drop a byte range from residency once it has been scanned.
    #[inline]
    fn dontneed(&self, start: usize, len: usize) {
        unsafe {
            ffi::madvise(self.ptr.add(start) as *mut c_void, len, ffi::MADV_DONTNEED);
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

// ---------------------------------------------------------------------
// Tokenizer tables: strict rule — A-Z (0x41-0x5A) lowers to a-z, a token
// is a maximal run of a-z0-9, every other byte (incl. all UTF-8 bytes
// >= 0x80) is a separator.
// ---------------------------------------------------------------------
struct TokTables {
    lower: [u8; 256],
    is_tok: [bool; 256],
}
const fn build_tables() -> TokTables {
    let mut lower = [0u8; 256];
    let mut is_tok = [false; 256];
    let mut b = 0usize;
    while b < 256 {
        let lo = if b >= 0x41 && b <= 0x5A { (b + 32) as u8 } else { b as u8 };
        lower[b] = lo;
        is_tok[b] = (lo >= b'a' && lo <= b'z') || (lo >= b'0' && lo <= b'9');
        b += 1;
    }
    TokTables { lower, is_tok }
}
static TABLES: TokTables = build_tables();

#[inline]
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= TABLES.lower[b as usize] as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

// ---------------------------------------------------------------------
// Needed-term table: built once from queries.txt, read-only during the
// parallel scan. Open addressing, linear probing.
// ---------------------------------------------------------------------
struct NeededTerms {
    slot_hash: Vec<u64>,
    slot_term: Vec<i32>, // -1 = empty
    mask: usize,
    bytes: Vec<Vec<u8>>,
    needs_pos: Vec<bool>,
}
impl NeededTerms {
    fn new(expected: usize) -> Self {
        let mut cap = 16usize;
        while cap < expected * 2 + 16 {
            cap *= 2;
        }
        NeededTerms {
            slot_hash: vec![0; cap],
            slot_term: vec![-1; cap],
            mask: cap - 1,
            bytes: Vec::new(),
            needs_pos: Vec::new(),
        }
    }
    /// Insert (or find) a term, returning its id. `pos` marks whether this
    /// occurrence requires positional tracking (phrase usage).
    fn intern(&mut self, term: &[u8], pos: bool) -> u32 {
        let h = fnv1a(term);
        let mut i = (h as usize) & self.mask;
        loop {
            let t = self.slot_term[i];
            if t == -1 {
                let id = self.bytes.len() as u32;
                self.slot_hash[i] = h;
                self.slot_term[i] = id as i32;
                self.bytes.push(term.to_vec());
                self.needs_pos.push(pos);
                return id;
            }
            if self.slot_hash[i] == h && self.bytes[t as usize] == term {
                if pos {
                    self.needs_pos[t as usize] = true;
                }
                return t as u32;
            }
            i = (i + 1) & self.mask;
        }
    }
    /// Read-only lookup used on the hot path during tokenizing.
    #[inline]
    fn lookup(&self, h: u64, term: &[u8]) -> i32 {
        let mut i = (h as usize) & self.mask;
        loop {
            let t = self.slot_term[i];
            if t == -1 {
                return -1;
            }
            if self.slot_hash[i] == h && self.bytes[t as usize].as_slice() == term {
                return t;
            }
            i = (i + 1) & self.mask;
        }
    }
    fn len(&self) -> usize {
        self.bytes.len()
    }
}

// ---------------------------------------------------------------------
// Parsed queries.
// ---------------------------------------------------------------------
#[derive(Clone, Copy, PartialEq)]
enum Op {
    And,
    Or,
    Not,
}
enum Query {
    Bool { first: u32, rest: Vec<(Op, u32)> },
    Phrase { terms: Vec<u32> },
}

fn parse_queries(raw: &[u8], needed: &mut NeededTerms) -> Vec<Query> {
    let mut out = Vec::new();
    for line in raw.split(|&b| b == b'\n') {
        let line = if line.last() == Some(&b'\r') { &line[..line.len() - 1] } else { line };
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix(b"BOOL ") {
            let mut words = rest.split(|&b| b == b' ');
            let first_w = words.next().expect("BOOL query needs at least one term");
            let first = needed.intern(first_w, false);
            let mut chain = Vec::new();
            loop {
                let op_w = match words.next() {
                    Some(w) => w,
                    None => break,
                };
                let op = match op_w {
                    b"AND" => Op::And,
                    b"OR" => Op::Or,
                    b"NOT" => Op::Not,
                    _ => panic!("bad operator {:?}", String::from_utf8_lossy(op_w)),
                };
                let term_w = words.next().expect("operator with no following term");
                let tid = needed.intern(term_w, false);
                chain.push((op, tid));
            }
            out.push(Query::Bool { first, rest: chain });
        } else if let Some(rest) = line.strip_prefix(b"PHRASE ") {
            let inner = &rest[1..rest.len() - 1]; // strip quotes
            let terms: Vec<u32> = inner
                .split(|&b| b == b' ')
                .map(|w| needed.intern(w, true))
                .collect();
            out.push(Query::Phrase { terms });
        } else {
            panic!("bad query line {:?}", String::from_utf8_lossy(line));
        }
    }
    out
}

// ---------------------------------------------------------------------
// Parallel scan: one occurrence record per (needed term, token hit).
// ---------------------------------------------------------------------
#[derive(Clone, Copy)]
struct Occ {
    term: u32,
    doc: u32,
    pos: u32,
}

/// Scan one line-aligned chunk, emitting Occ records in ascending
/// (doc, pos) order (guaranteed by scanning left to right).
///
/// Each line in docs.txt has the format: `<doc_id>\t<content>\n`
/// We parse the real doc_id from before the tab, and only tokenize
/// the content after the tab.
fn scan_chunk(
    data: &[u8],
    _chunk_start_doc: u32,
    needed: &NeededTerms,
    last_doc_scratch: &mut [u32],
    lower_scratch: &mut Vec<u8>,
    out: &mut Vec<Occ>,
) {
    let n = data.len();
    let mut i = 0usize;
    while i < n {
        // Parse the doc id (digits before the tab).
        let mut doc: u32 = 0;
        while i < n && data[i] != b'\t' && data[i] != b'\n' {
            if data[i] >= b'0' && data[i] <= b'9' {
                doc = doc * 10 + (data[i] - b'0') as u32;
            }
            i += 1;
        }
        if i >= n || data[i] == b'\n' {
            i += 1;
            continue;
        }
        // Skip the tab.
        i += 1;
        // Now tokenize content until newline.
        let mut pos: u32 = 0;
        while i < n && data[i] != b'\n' {
            let b = data[i];
            if !TABLES.is_tok[b as usize] {
                i += 1;
                continue;
            }
            // Scan the full token, hashing as we go, no allocation.
            let start = i;
            let mut h: u64 = 0xcbf29ce484222325;
            while i < n && data[i] != b'\n' && TABLES.is_tok[data[i] as usize] {
                h ^= TABLES.lower[data[i] as usize] as u64;
                h = h.wrapping_mul(0x100000001b3);
                i += 1;
            }
            let raw_slice = &data[start..i];
            let tid = if raw_has_upper(raw_slice) {
                lower_scratch.clear();
                lower_scratch.extend(raw_slice.iter().map(|&b| TABLES.lower[b as usize]));
                needed.lookup(h, lower_scratch)
            } else {
                needed.lookup(h, raw_slice)
            };
            if tid >= 0 {
                let t = tid as usize;
                if needed.needs_pos[t] {
                    out.push(Occ { term: tid as u32, doc, pos });
                } else if last_doc_scratch[t] != doc {
                    last_doc_scratch[t] = doc;
                    out.push(Occ { term: tid as u32, doc, pos: 0 });
                }
            }
            pos += 1;
        }
        // Skip the newline.
        if i < n && data[i] == b'\n' {
            i += 1;
        }
    }
}

#[inline]
fn raw_has_upper(s: &[u8]) -> bool {
    s.iter().any(|&b| b >= 0x41 && b <= 0x5A)
}

/// Count newlines in a byte range (used to assign global doc ids to
/// chunk boundaries without a cross-thread atomic counter).
fn count_newlines(data: &[u8]) -> usize {
    bytecount_nl(data)
}
#[inline]
fn bytecount_nl(data: &[u8]) -> usize {
    data.iter().filter(|&&b| b == b'\n').count()
}

struct ChunkPlan {
    start: usize,
    end: usize,
    start_doc: u32,
}

fn plan_chunks(data: &[u8], target_chunks: usize) -> Vec<ChunkPlan> {
    let n = data.len();
    if n == 0 {
        return Vec::new();
    }
    let approx = (n / target_chunks.max(1)).max(1);
    let mut bounds = Vec::new();
    let mut pos = 0usize;
    while pos < n {
        let mut end = (pos + approx).min(n);
        if end < n {
            while end < n && data[end - 1] != b'\n' {
                end += 1;
            }
        }
        bounds.push((pos, end));
        pos = end;
    }
    // Prefix-sum newline counts to get each chunk's starting doc id.
    let mut plans = Vec::with_capacity(bounds.len());
    let mut doc_acc: u32 = 0;
    for &(s, e) in &bounds {
        plans.push(ChunkPlan { start: s, end: e, start_doc: doc_acc });
        doc_acc += count_newlines(&data[s..e]) as u32;
    }
    plans
}

// ---------------------------------------------------------------------
// Final per-term arrays after the counting-sort merge.
// ---------------------------------------------------------------------
struct TermArrays {
    starts: Vec<u32>,
    ends: Vec<u32>,
    doc_ids: Vec<u32>, // raw, ascending within each term's range
    positions: Vec<u32>,
    unique_docs: Vec<Vec<u32>>, // deduped, for BOOL evaluation
}

fn build_term_arrays(m: usize, chunk_bufs: &[Vec<Occ>]) -> TermArrays {
    let mut counts = vec![0u32; m];
    for buf in chunk_bufs {
        for occ in buf {
            counts[occ.term as usize] += 1;
        }
    }
    let mut starts = vec![0u32; m + 1];
    for t in 0..m {
        starts[t + 1] = starts[t] + counts[t];
    }
    let total = starts[m] as usize;
    let mut doc_ids = vec![0u32; total];
    let mut positions = vec![0u32; total];
    let mut cursor = starts.clone();
    for buf in chunk_bufs {
        for occ in buf {
            let idx = cursor[occ.term as usize] as usize;
            doc_ids[idx] = occ.doc;
            positions[idx] = occ.pos;
            cursor[occ.term as usize] += 1;
        }
    }
    let ends: Vec<u32> = starts[1..].to_vec();
    let starts_lo: Vec<u32> = starts[..m].to_vec();
    let mut unique_docs = Vec::with_capacity(m);
    for t in 0..m {
        let s = starts_lo[t] as usize;
        let e = ends[t] as usize;
        let mut v: Vec<u32> = Vec::with_capacity(e - s);
        let mut last = u32::MAX;
        for &d in &doc_ids[s..e] {
            if d != last {
                v.push(d);
                last = d;
            }
        }
        unique_docs.push(v);
    }
    TermArrays { starts: starts_lo, ends, doc_ids, positions, unique_docs }
}

// ---------------------------------------------------------------------
// BOOL / PHRASE evaluation.
// ---------------------------------------------------------------------
#[inline]
fn merge_and(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    out.clear();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            out.push(a[i]);
            i += 1;
            j += 1;
        } else if a[i] < b[j] {
            i += 1;
        } else {
            j += 1;
        }
    }
}
#[inline]
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
#[inline]
fn merge_not(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    out.clear();
    let (mut i, mut j) = (0, 0);
    while i < a.len() {
        if j < b.len() && b[j] < a[i] {
            j += 1;
        } else if j < b.len() && b[j] == a[i] {
            i += 1;
            j += 1;
        } else {
            out.push(a[i]);
            i += 1;
        }
    }
}

struct Summary {
    count: u64,
    sum: u64,
    first10: [u32; 10],
    n10: usize,
}
#[inline]
fn summarize(ids: &[u32]) -> Summary {
    let mut s = Summary { count: ids.len() as u64, sum: 0, first10: [0; 10], n10: ids.len().min(10) };
    for (k, &id) in ids.iter().enumerate() {
        s.sum += id as u64;
        if k < 10 {
            s.first10[k] = id;
        }
    }
    s
}

struct Scratch {
    buf_a: Vec<u32>,
    buf_b: Vec<u32>,
}

fn eval_bool(first: u32, rest: &[(Op, u32)], ta: &TermArrays, sc: &mut Scratch) -> Summary {
    let mut cur: &[u32] = &ta.unique_docs[first as usize];
    let mut use_a = true;
    for &(op, t) in rest {
        let next = &ta.unique_docs[t as usize];
        let out = if use_a { &mut sc.buf_a } else { &mut sc.buf_b };
        match op {
            Op::And => merge_and(cur, next, out),
            Op::Or => merge_or(cur, next, out),
            Op::Not => merge_not(cur, next, out),
        }
        // SAFETY-free trick: copy the just-written buffer's pointer out as
        // `cur` for the next round by swapping which buffer is "current".
        cur = unsafe { std::slice::from_raw_parts(out.as_ptr(), out.len()) };
        use_a = !use_a;
    }
    summarize(cur)
}

fn eval_phrase(terms: &[u32], ta: &TermArrays) -> Summary {
    let k = terms.len();
    if k == 1 {
        return summarize(&ta.unique_docs[terms[0] as usize]);
    }
    // Pick the shortest occurrence list as the driver.
    let mut driver_idx = 0usize;
    let mut driver_len = usize::MAX;
    for (i, &t) in terms.iter().enumerate() {
        let len = (ta.ends[t as usize] - ta.starts[t as usize]) as usize;
        if len < driver_len {
            driver_len = len;
            driver_idx = i;
        }
    }
    let dterm = terms[driver_idx] as usize;
    let ds = ta.starts[dterm] as usize;
    let de = ta.ends[dterm] as usize;
    let mut matches: Vec<u32> = Vec::new();
    let mut last_doc = u32::MAX;
    'outer: for idx in ds..de {
        let doc = ta.doc_ids[idx];
        if doc == last_doc {
            continue;
        }
        let p = ta.positions[idx];
        if p < driver_idx as u32 {
            continue; // phrase would start before token 0
        }
        let start = p - driver_idx as u32;
        for (i, &t) in terms.iter().enumerate() {
            if i == driver_idx {
                continue;
            }
            let need_pos = start + i as u32;
            let t = t as usize;
            let s = ta.starts[t] as usize;
            let e = ta.ends[t] as usize;
            let slice_doc = &ta.doc_ids[s..e];
            let slice_pos = &ta.positions[s..e];
            // Binary search for (doc, need_pos) in this term's occurrence list.
            let mut lo = 0usize;
            let mut hi = slice_doc.len();
            let mut found = false;
            while lo < hi {
                let mid = (lo + hi) / 2;
                let key = (slice_doc[mid], slice_pos[mid]);
                if key == (doc, need_pos) {
                    found = true;
                    break;
                } else if key < (doc, need_pos) {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            if !found {
                continue 'outer;
            }
        }
        matches.push(doc);
        last_doc = doc;
    }
    matches.sort_unstable();
    summarize(&matches)
}

// ---------------------------------------------------------------------
// Output formatting (no `format!`/`write!` per line — manual itoa into
// one growable byte buffer).
// ---------------------------------------------------------------------
#[inline]
fn push_u64(buf: &mut Vec<u8>, mut v: u64) {
    if v == 0 {
        buf.push(b'0');
        return;
    }
    let mut tmp = [0u8; 20];
    let mut i = 20;
    while v > 0 {
        i -= 1;
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    buf.extend_from_slice(&tmp[i..]);
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

// ---------------------------------------------------------------------
fn main() {
    let t0 = Instant::now();
    let args: Vec<String> = env::args().collect();
    let mut data_dir = "/data".to_string();
    let mut out_dir = "/out".to_string();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--data" => {
                data_dir = args[i + 1].clone();
                i += 2;
            }
            "--out" => {
                out_dir = args[i + 1].clone();
                i += 2;
            }
            _ => {
                i += 1;
            }
        }
    }

    // 1. Queries first — this decides what we even bother indexing.
    let queries_raw = fs::read(format!("{}/queries.txt", data_dir)).expect("read queries.txt");
    let mut needed = NeededTerms::new(4096);
    let queries = parse_queries(&queries_raw, &mut needed);
    drop(queries_raw);
    let m = needed.len();

    // 2. mmap the corpus and plan line-aligned chunks for work-stealing.
    let mmap = Mmap::open(&format!("{}/docs.txt", data_dir));
    let data = mmap.as_slice();
    let n_threads = std::thread::available_parallelism().map(|p| p.get()).unwrap_or(4).min(12);
    let target_chunks = (n_threads * 24).max(1);
    let plans = plan_chunks(data, target_chunks);

    // 3. Parallel scan with work-stealing + per-chunk madvise(DONTNEED).
    let next_chunk = AtomicUsize::new(0);
    let chunk_results: Vec<std::sync::Mutex<Vec<Occ>>> =
        (0..plans.len()).map(|_| std::sync::Mutex::new(Vec::new())).collect();

    std::thread::scope(|scope| {
        for _ in 0..n_threads {
            let plans_ref = &plans;
            let needed_ref = &needed;
            let next_chunk_ref = &next_chunk;
            let chunk_results_ref = &chunk_results;
            let mmap_ref = &mmap;
            scope.spawn(move || {
                let mut last_doc_scratch = vec![u32::MAX; m];
                let mut lower_scratch: Vec<u8> = Vec::with_capacity(64);
                loop {
                    let idx = next_chunk_ref.fetch_add(1, Ordering::Relaxed);
                    if idx >= plans_ref.len() {
                        break;
                    }
                    let plan = &plans_ref[idx];
                    let len = plan.end - plan.start;
                    if len == 0 {
                        continue;
                    }
                    mmap_ref.willneed(plan.start, len);
                    let slice = &data[plan.start..plan.end];
                    let mut local = Vec::new();
                    for v in last_doc_scratch.iter_mut() {
                        *v = u32::MAX;
                    }
                    scan_chunk(
                        slice,
                        plan.start_doc,
                        needed_ref,
                        &mut last_doc_scratch,
                        &mut lower_scratch,
                        &mut local,
                    );
                    mmap_ref.dontneed(plan.start, len);
                    *chunk_results_ref[idx].lock().unwrap() = local;
                }
            });
        }
    });

    let chunk_bufs: Vec<Vec<Occ>> = chunk_results.into_iter().map(|m| m.into_inner().unwrap()).collect();
    let ta = build_term_arrays(m, &chunk_bufs);
    drop(chunk_bufs);
    drop(mmap); // corpus no longer needed at all past this point

    // 4. Answer every query into one output buffer, no per-query allocation.
    let mut out = Vec::with_capacity(queries.len() * 24);
    let mut sc = Scratch { buf_a: Vec::new(), buf_b: Vec::new() };
    for q in &queries {
        let summary = match q {
            Query::Bool { first, rest } => eval_bool(*first, rest, &ta, &mut sc),
            Query::Phrase { terms } => eval_phrase(terms, &ta),
        };
        write_summary(&mut out, &summary);
    }

    fs::create_dir_all(&out_dir).expect("create out dir");
    fs::write(format!("{}/results.txt", out_dir), &out).expect("write results.txt");

    if env::var("SPEED_DEMON_DEBUG").is_ok() {
        eprintln!(
            "needed_terms={} chunks={} threads={} elapsed={:?}",
            m,
            plans.len(),
            n_threads,
            t0.elapsed()
        );
    }
}
