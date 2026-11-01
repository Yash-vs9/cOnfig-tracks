#![allow(non_snake_case)]
#![allow(unused)]

use std::fs::File;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

const NUM_THREADS: usize = 12;
const BLOCK_DOCS: usize = 65536;
const BITSET_WORDS: usize = BLOCK_DOCS / 64; // 1024 u64 words = 8 KiB
const MAX_HEAVY: usize = 1536;
const HT_CAP: usize = 262144; // 2^18 slots (load factor < 0.30)
const HT_MASK: usize = HT_CAP - 1;
const EMPTY_SLOT: u32 = u32::MAX;
const FLAG_LE8: u32 = 0x8000_0000;
const ID_MASK: u32 = 0x7FFF_FFFF;
const SPARSE_LIMIT: usize = 512;

extern "C" {
    fn mmap(
        addr: *mut core::ffi::c_void,
        len: usize,
        prot: i32,
        flags: i32,
        fd: i32,
        offset: i64,
    ) -> *mut core::ffi::c_void;
}

struct Mmap {
    ptr: *const u8,
    len: usize,
}
unsafe impl Sync for Mmap {}
unsafe impl Send for Mmap {}

impl Mmap {
    fn open(path: &str) -> Self {
        let f = File::open(path).expect("open failed");
        let len = f.metadata().expect("meta failed").len() as usize;
        if len == 0 {
            return Self { ptr: ptr::null(), len: 0 };
        }
        let ptr = unsafe { mmap(ptr::null_mut(), len, 1, 1, f.as_raw_fd(), 0) };
        assert!(ptr as isize != -1, "mmap failed");
        Self { ptr: ptr as *const u8, len }
    }
    #[inline(always)]
    fn as_slice(&self) -> &[u8] {
        if self.len == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
        }
    }
}

static CHAR_MAP: [u8; 256] = {
    let mut table = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        if i >= b'0' as usize && i <= b'9' as usize {
            table[i] = i as u8;
        } else if i >= b'a' as usize && i <= b'z' as usize {
            table[i] = i as u8;
        } else if i >= b'A' as usize && i <= b'Z' as usize {
            table[i] = (i as u8) + 32;
        }
        i += 1;
    }
    table
};

#[inline(always)]
fn hash_words(w0: u64, w1: u64, w2: u64, w3: u64, len: usize) -> usize {
    let mut h = w0.wrapping_mul(0x9E3779B97F4A7C15);
    if len > 8 {
        h = (h ^ w1).wrapping_mul(0xBF58476D1CE4E5B9);
        if len > 16 {
            h = (h ^ w2 ^ w3.rotate_left(31)).wrapping_mul(0x94D049BB133111EB);
        }
    }
    (h ^ (h >> 32) ^ (len as u64)) as usize
}

#[inline(always)]
fn pack_lowercase_slice(s: &[u8]) -> (u64, u64, u64, u64, usize) {
    let len = s.len();
    let mut w0 = 0u64;
    let mut w1 = 0u64;
    let mut w2 = 0u64;
    let mut w3 = 0u64;
    let n0 = len.min(8);
    for &b in &s[0..n0] {
        w0 = (w0 << 8) | (b as u64);
    }
    if len > 8 {
        let n1 = len.min(16);
        for &b in &s[8..n1] {
            w1 = (w1 << 8) | (b as u64);
        }
        if len > 16 {
            let n2 = len.min(24);
            for &b in &s[16..n2] {
                w2 = (w2 << 8) | (b as u64);
            }
            if len > 24 {
                let n3 = len.min(32);
                for &b in &s[24..n3] {
                    w3 = (w3 << 8) | (b as u64);
                }
            }
        }
    }
    (w0, w1, w2, w3, len)
}

#[derive(Clone, Copy)]
struct TermWords {
    w0: u64,
    w1: u64,
    w2: u64,
    w3: u64,
    len: u8,
}

struct TermTable {
    slots_w0: Vec<u64>,
    slots_id: Vec<u32>,
    terms: Vec<TermWords>,
    repeat_term: Vec<u32>, // u32::MAX if no repeat virtual term
    query_freq: Vec<u32>,
    is_heavy: Vec<u16>,    // u16::MAX if sparse, else 0..num_heavy
    sparse_idx: Vec<u32>,  // index in 0..num_sparse if sparse
    num_heavy: usize,
    num_sparse: usize,
}

impl TermTable {
    fn new() -> Self {
        Self {
            slots_w0: vec![0; HT_CAP],
            slots_id: vec![EMPTY_SLOT; HT_CAP],
            terms: Vec::with_capacity(81920),
            repeat_term: Vec::with_capacity(81920),
            query_freq: Vec::with_capacity(81920),
            is_heavy: Vec::new(),
            sparse_idx: Vec::new(),
            num_heavy: 0,
            num_sparse: 0,
        }
    }

    fn intern(&mut self, s: &[u8]) -> u32 {
        let (w0, w1, w2, w3, len) = pack_lowercase_slice(s);
        let mut idx = hash_words(w0, w1, w2, w3, len) & HT_MASK;
        loop {
            let raw_id = self.slots_id[idx];
            if raw_id == EMPTY_SLOT {
                let id = self.terms.len() as u32;
                self.terms.push(TermWords { w0, w1, w2, w3, len: len as u8 });
                self.repeat_term.push(u32::MAX);
                self.query_freq.push(1);
                self.slots_w0[idx] = w0;
                self.slots_id[idx] = if len <= 8 { id | FLAG_LE8 } else { id };
                return id;
            }
            if self.slots_w0[idx] == w0 {
                let id = (raw_id & ID_MASK) as usize;
                if len <= 8 && (raw_id & FLAG_LE8) != 0 {
                    self.query_freq[id] += 1;
                    return id as u32;
                }
                let t = &self.terms[id];
                if t.len as usize == len && t.w1 == w1 && t.w2 == w2 && t.w3 == w3 {
                    self.query_freq[id] += 1;
                    return id as u32;
                }
            }
            idx = (idx + 1) & HT_MASK;
        }
    }

    fn get_or_create_repeat(&mut self, base_id: u32) -> u32 {
        let existing = self.repeat_term[base_id as usize];
        if existing != u32::MAX {
            return existing;
        }
        let id = self.terms.len() as u32;
        let base_t = self.terms[base_id as usize];
        self.terms.push(base_t);
        self.repeat_term.push(u32::MAX);
        self.query_freq.push(1);
        self.repeat_term[base_id as usize] = id;
        id
    }

    #[inline(always)]
    fn lookup(&self, w0: u64, w1: u64, w2: u64, w3: u64, len: usize) -> u32 {
        let mut idx = hash_words(w0, w1, w2, w3, len) & HT_MASK;
        loop {
            let raw_id = unsafe { *self.slots_id.get_unchecked(idx) };
            if raw_id == EMPTY_SLOT {
                return EMPTY_SLOT;
            }
            if unsafe { *self.slots_w0.get_unchecked(idx) } == w0 {
                if len <= 8 && (raw_id & FLAG_LE8) != 0 {
                    return raw_id & ID_MASK;
                }
                let id = (raw_id & ID_MASK) as usize;
                let t = unsafe { self.terms.get_unchecked(id) };
                if t.len as usize == len && t.w1 == w1 && t.w2 == w2 && t.w3 == w3 {
                    return id as u32;
                }
            }
            idx = (idx + 1) & HT_MASK;
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum BoolOp {
    And = 0,
    Or = 1,
    Not = 2,
}

#[derive(Clone, Copy)]
struct BoolQuery {
    num_terms: u8,
    and_prefix_len: u8,
    terms: [u32; 6],
    ops: [BoolOp; 5],
}

#[derive(Clone, Copy)]
struct PhraseQuery {
    num_terms: u8,
    num_filters: u8,
    terms: [u32; 8],
    filter_terms: [u32; 12],
    kmp_pi: [u8; 8],
}

#[derive(Clone, Copy)]
enum Query {
    Bool(BoolQuery),
    Phrase(PhraseQuery),
}

#[derive(Clone, Copy)]
struct QueryResult {
    count: u32,
    sum: u64,
    top_len: u8,
    top10: [u32; 10],
}

impl QueryResult {
    #[inline(always)]
    fn new() -> Self {
        Self {
            count: 0,
            sum: 0,
            top_len: 0,
            top10: [0; 10],
        }
    }
}

fn parse_queries(data: &[u8], table: &mut TermTable) -> Vec<Query> {
    let mut queries = Vec::with_capacity(50000);
    let mut pos = 0;
    let end = data.len();

    while pos < end {
        let line_start = pos;
        while pos < end && data[pos] != b'\n' {
            pos += 1;
        }
        let line = &data[line_start..pos];
        pos += 1;
        if line.is_empty() {
            continue;
        }

        if line[0] == b'B' {
            // BOOL t1 [OP t2 ...]
            let mut p = 5; // skip "BOOL "
            let mut terms = [0u32; 6];
            let mut ops = [BoolOp::And; 5];
            let mut n_terms = 0usize;

            while p < line.len() {
                let ts = p;
                while p < line.len() && line[p] != b' ' {
                    p += 1;
                }
                terms[n_terms] = table.intern(&line[ts..p]);
                n_terms += 1;
                if p >= line.len() {
                    break;
                }
                p += 1; // skip space
                let os = p;
                while p < line.len() && line[p] != b' ' {
                    p += 1;
                }
                let op_bytes = &line[os..p];
                ops[n_terms - 1] = match op_bytes[0] {
                    b'A' => BoolOp::And,
                    b'O' => BoolOp::Or,
                    _ => BoolOp::Not,
                };
                p += 1; // skip space
            }

            let mut and_prefix_len = 1u8;
            while (and_prefix_len as usize) < n_terms
                && ops[(and_prefix_len as usize) - 1] == BoolOp::And
            {
                and_prefix_len += 1;
            }

            queries.push(Query::Bool(BoolQuery {
                num_terms: n_terms as u8,
                and_prefix_len,
                terms,
                ops,
            }));
        } else {
            // PHRASE "t1 t2 ... tk"
            let mut p = 8; // skip 'PHRASE "'
            let line_end = line.len() - 1; // exclude trailing '"'
            let mut terms = [0u32; 8];
            let mut n_terms = 0usize;

            while p < line_end {
                let ts = p;
                while p < line_end && line[p] != b' ' {
                    p += 1;
                }
                terms[n_terms] = table.intern(&line[ts..p]);
                n_terms += 1;
                p += 1;
            }

            // Deduplicated filter terms + virtual repeat terms for adjacent identical tokens
            let mut filter_terms = [0u32; 12];
            let mut n_filters = 0usize;
            for i in 0..n_terms {
                let tid = terms[i];
                let mut seen = false;
                for j in 0..n_filters {
                    if filter_terms[j] == tid {
                        seen = true;
                        break;
                    }
                }
                if !seen {
                    filter_terms[n_filters] = tid;
                    n_filters += 1;
                }
                if i + 1 < n_terms && terms[i + 1] == tid {
                    let rep_id = table.get_or_create_repeat(tid);
                    let mut rep_seen = false;
                    for j in 0..n_filters {
                        if filter_terms[j] == rep_id {
                            rep_seen = true;
                            break;
                        }
                    }
                    if !rep_seen && n_filters < 12 {
                        filter_terms[n_filters] = rep_id;
                        n_filters += 1;
                    }
                }
            }

            // Precompute KMP prefix table over phrase terms
            let mut kmp_pi = [0u8; 8];
            for i in 1..n_terms {
                let mut j = kmp_pi[i - 1] as usize;
                while j > 0 && terms[i] != terms[j] {
                    j = kmp_pi[j - 1] as usize;
                }
                if terms[i] == terms[j] {
                    j += 1;
                }
                kmp_pi[i] = j as u8;
            }

            queries.push(Query::Phrase(PhraseQuery {
                num_terms: n_terms as u8,
                num_filters: n_filters as u8,
                terms,
                filter_terms,
                kmp_pi,
            }));
        }
    }
    queries
}

#[inline(always)]
fn scan_next_token(text: &[u8], mut i: usize) -> Option<(usize, u64, u64, u64, u64, usize)> {
    let end = text.len();
    while i < end {
        let m0 = unsafe { *CHAR_MAP.get_unchecked(*text.get_unchecked(i) as usize) };
        if m0 != 0 {
            let mut w0 = m0 as u64;
            i += 1;
            let limit8 = (i + 7).min(end);
            let start_after_first = i;
            while i < limit8 {
                let m = unsafe { *CHAR_MAP.get_unchecked(*text.get_unchecked(i) as usize) };
                if m == 0 {
                    let len = 1 + (i - start_after_first);
                    return Some((i + 1, w0, 0, 0, 0, len));
                }
                w0 = (w0 << 8) | (m as u64);
                i += 1;
            }
            let mut len = 1 + (i - start_after_first);
            let mut w1 = 0u64;
            let mut w2 = 0u64;
            let mut w3 = 0u64;
            while i < end {
                let m = unsafe { *CHAR_MAP.get_unchecked(*text.get_unchecked(i) as usize) };
                if m == 0 {
                    i += 1;
                    break;
                }
                if len < 16 {
                    w1 = (w1 << 8) | (m as u64);
                } else if len < 24 {
                    w2 = (w2 << 8) | (m as u64);
                } else if len < 32 {
                    w3 = (w3 << 8) | (m as u64);
                }
                len += 1;
                i += 1;
            }
            return Some((i, w0, w1, w2, w3, len));
        }
        i += 1;
    }
    None
}

fn classify_terms(docs: &[u8], doc_offsets: &[u32], table: &mut TermTable) {
    let num_terms = table.terms.len();
    let num_docs = doc_offsets.len().saturating_sub(1);
    let mut sample_df = vec![0u32; num_terms];
    let mut seen_stamp = vec![0u16; num_terms];

    let samples = num_docs.min(512);
    let step = if samples > 0 { (num_docs / samples).max(1) } else { 1 };

    let mut stamp = 1u16;
    let mut d = 0;
    while d < num_docs && (stamp as usize) <= samples {
        let mut s = doc_offsets[d] as usize;
        let e = (doc_offsets[d + 1] as usize).saturating_sub(1);
        while s < e && docs[s] != b'\t' {
            s += 1;
        }
        s = (s + 1).min(e);
        let text = &docs[s..e];
        let mut pos = 0;
        while let Some((next_pos, w0, w1, w2, w3, len)) = scan_next_token(text, pos) {
            pos = next_pos;
            if len <= 32 {
                let tid = table.lookup(w0, w1, w2, w3, len);
                if tid != EMPTY_SLOT {
                    let idx = tid as usize;
                    if seen_stamp[idx] != stamp {
                        seen_stamp[idx] = stamp;
                        sample_df[idx] += 1;
                    }
                }
            }
        }
        stamp += 1;
        d += step;
    }

    // Score terms by sampled document frequency + query frequency; repeat virtual terms stay sparse
    let mut ranked: Vec<(u64, usize)> = (0..num_terms)
        .map(|i| {
            let score = (sample_df[i] as u64) * 64 + (table.query_freq[i] as u64);
            (score, i)
        })
        .collect();
    ranked.sort_unstable_by(|a, b| b.0.cmp(&a.0));

    let num_heavy = num_terms.min(MAX_HEAVY);
    table.is_heavy = vec![u16::MAX; num_terms];
    table.sparse_idx = vec![0u32; num_terms];

    let mut heavy_count = 0usize;
    for &(_score, tid) in &ranked {
        if heavy_count < num_heavy {
            table.is_heavy[tid] = heavy_count as u16;
            heavy_count += 1;
        }
    }
    let mut sparse_count = 0u32;
    for tid in 0..num_terms {
        if table.is_heavy[tid] == u16::MAX {
            table.sparse_idx[tid] = sparse_count;
            sparse_count += 1;
        }
    }
    table.num_heavy = heavy_count;
    table.num_sparse = sparse_count as usize;
}

#[inline(always)]
fn doc_text_slice<'a>(docs: &'a [u8], doc_offsets: &[u32], doc_id: usize) -> &'a [u8] {
    let mut s = unsafe { *doc_offsets.get_unchecked(doc_id) as usize };
    let e = unsafe { (*doc_offsets.get_unchecked(doc_id + 1) as usize).saturating_sub(1) };
    while s < e && unsafe { *docs.get_unchecked(s) } != b'\t' {
        s += 1;
    }
    s = (s + 1).min(e);
    unsafe { docs.get_unchecked(s..e) }
}

#[inline(always)]
fn verify_phrase_in_doc(
    docs: &[u8],
    doc_offsets: &[u32],
    table: &TermTable,
    pq: &PhraseQuery,
    global_doc_id: usize,
) -> bool {
    let text = doc_text_slice(docs, doc_offsets, global_doc_id);
    let k = pq.num_terms as usize;
    let mut p_words = [TermWords { w0: 0, w1: 0, w2: 0, w3: 0, len: 0 }; 8];
    for idx in 0..k {
        p_words[idx] = unsafe { *table.terms.get_unchecked(pq.terms[idx] as usize) };
    }

    let mut j = 0usize;
    let mut pos = 0usize;
    while let Some((next_pos, w0, w1, w2, w3, len)) = scan_next_token(text, pos) {
        pos = next_pos;
        loop {
            let pw = unsafe { p_words.get_unchecked(j) };
            let matched = (pw.len as usize == len)
                && (pw.w0 == w0)
                && (len <= 8 || (pw.w1 == w1 && pw.w2 == w2 && pw.w3 == w3));
            if matched {
                j += 1;
                if j == k {
                    return true;
                }
                break;
            }
            if j == 0 {
                break;
            }
            j = pq.kmp_pi[j - 1] as usize;
        }
    }
    false
}

#[inline(always)]
fn intersect_sorted_u16(a: &[u16], b: &[u16], out: &mut [u16]) -> usize {
    let mut i = 0usize;
    let mut j = 0usize;
    let mut k = 0usize;
    let na = a.len();
    let nb = b.len();
    while i < na && j < nb {
        let va = unsafe { *a.get_unchecked(i) };
        let vb = unsafe { *b.get_unchecked(j) };
        if va == vb {
            unsafe { *out.get_unchecked_mut(k) = va };
            k += 1;
            i += 1;
            j += 1;
        } else if va < vb {
            i += 1;
        } else {
            j += 1;
        }
    }
    k
}

#[inline(always)]
fn difference_sorted_u16(a: &[u16], b: &[u16], out: &mut [u16]) -> usize {
    let mut i = 0usize;
    let mut j = 0usize;
    let mut k = 0usize;
    let na = a.len();
    let nb = b.len();
    while i < na && j < nb {
        let va = unsafe { *a.get_unchecked(i) };
        let vb = unsafe { *b.get_unchecked(j) };
        if va < vb {
            unsafe { *out.get_unchecked_mut(k) = va };
            k += 1;
            i += 1;
        } else if va == vb {
            i += 1;
            j += 1;
        } else {
            j += 1;
        }
    }
    while i < na {
        unsafe { *out.get_unchecked_mut(k) = *a.get_unchecked(i) };
        k += 1;
        i += 1;
    }
    k
}

#[inline(always)]
fn union_sorted_u16(a: &[u16], b: &[u16], out: &mut [u16]) -> usize {
    let mut i = 0usize;
    let mut j = 0usize;
    let mut k = 0usize;
    let na = a.len();
    let nb = b.len();
    while i < na && j < nb {
        let va = unsafe { *a.get_unchecked(i) };
        let vb = unsafe { *b.get_unchecked(j) };
        if va < vb {
            unsafe { *out.get_unchecked_mut(k) = va };
            k += 1;
            i += 1;
        } else if vb < va {
            unsafe { *out.get_unchecked_mut(k) = vb };
            k += 1;
            j += 1;
        } else {
            unsafe { *out.get_unchecked_mut(k) = va };
            k += 1;
            i += 1;
            j += 1;
        }
    }
    while i < na {
        unsafe { *out.get_unchecked_mut(k) = *a.get_unchecked(i) };
        k += 1;
        i += 1;
    }
    while j < nb {
        unsafe { *out.get_unchecked_mut(k) = *b.get_unchecked(j) };
        k += 1;
        j += 1;
    }
    k
}

enum TermRef<'a> {
    Heavy(&'a [u64]),
    Sparse(&'a [u16]),
}

impl<'a> TermRef<'a> {
    #[inline(always)]
    fn est_len(&self) -> usize {
        match self {
            TermRef::Sparse(s) => s.len(),
            TermRef::Heavy(_) => usize::MAX,
        }
    }
}

#[inline(always)]
fn get_term_ref<'a>(
    tid: u32,
    table: &TermTable,
    heavy_bitsets: &'a [u64],
    num_words: usize,
    csr_offsets: &[u32],
    csr_data: &'a [u16],
) -> TermRef<'a> {
    let h = unsafe { *table.is_heavy.get_unchecked(tid as usize) };
    if h != u16::MAX {
        let start = (h as usize) * BITSET_WORDS;
        TermRef::Heavy(unsafe { heavy_bitsets.get_unchecked(start..start + num_words) })
    } else {
        let s_idx = unsafe { *table.sparse_idx.get_unchecked(tid as usize) as usize };
        let start = unsafe { *csr_offsets.get_unchecked(s_idx) as usize };
        let end = unsafe { *csr_offsets.get_unchecked(s_idx + 1) as usize };
        TermRef::Sparse(unsafe { csr_data.get_unchecked(start..end) })
    }
}

fn eval_bool_query_block(
    bq: &BoolQuery,
    table: &TermTable,
    heavy_bitsets: &[u64],
    num_words: usize,
    csr_offsets: &[u32],
    csr_data: &[u16],
    block_start_doc: u32,
    res: &mut QueryResult,
    bs_buf: &mut [u64; BITSET_WORDS],
) {
    let n_terms = bq.num_terms as usize;
    let prefix_len = bq.and_prefix_len as usize;

    // Pick rarest term in leading AND-prefix
    let mut best_idx = 0usize;
    let mut best_ref = get_term_ref(
        bq.terms[0],
        table,
        heavy_bitsets,
        num_words,
        csr_offsets,
        csr_data,
    );
    let mut best_len = best_ref.est_len();

    for i in 1..prefix_len {
        let r = get_term_ref(
            bq.terms[i],
            table,
            heavy_bitsets,
            num_words,
            csr_offsets,
            csr_data,
        );
        let l = r.est_len();
        if l < best_len {
            best_len = l;
            best_idx = i;
            best_ref = r;
        }
    }

    let mut buf_a = [0u16; SPARSE_LIMIT];
    let mut buf_b = [0u16; SPARSE_LIMIT];
    let mut sp_len = 0usize;
    let mut in_bitset = false;

    match best_ref {
        TermRef::Sparse(s) if s.len() <= SPARSE_LIMIT => {
            sp_len = s.len();
            buf_a[..sp_len].copy_from_slice(s);
        }
        TermRef::Sparse(s) => {
            in_bitset = true;
            for w in &mut bs_buf[..num_words] {
                *w = 0;
            }
            for &d in s {
                unsafe {
                    *bs_buf.get_unchecked_mut((d as usize) >> 6) |= 1u64 << (d & 63);
                }
            }
        }
        TermRef::Heavy(hb) => {
            in_bitset = true;
            bs_buf[..num_words].copy_from_slice(hb);
        }
    }

    // Intersect remaining terms in the commutative AND-prefix
    for i in 0..prefix_len {
        if i == best_idx {
            continue;
        }
        if !in_bitset && sp_len == 0 {
            break;
        }
        let tref = get_term_ref(
            bq.terms[i],
            table,
            heavy_bitsets,
            num_words,
            csr_offsets,
            csr_data,
        );
        if !in_bitset {
            match tref {
                TermRef::Heavy(hb) => {
                    let mut w = 0usize;
                    for r in 0..sp_len {
                        let d = buf_a[r];
                        if (unsafe { *hb.get_unchecked((d as usize) >> 6) } >> (d & 63)) & 1 != 0 {
                            buf_a[w] = d;
                            w += 1;
                        }
                    }
                    sp_len = w;
                }
                TermRef::Sparse(s) => {
                    let new_len = intersect_sorted_u16(&buf_a[..sp_len], s, &mut buf_b);
                    buf_a[..new_len].copy_from_slice(&buf_b[..new_len]);
                    sp_len = new_len;
                }
            }
        } else {
            match tref {
                TermRef::Heavy(hb) => {
                    for w in 0..num_words {
                        bs_buf[w] &= hb[w];
                    }
                }
                TermRef::Sparse(s) => {
                    if s.len() <= SPARSE_LIMIT {
                        let mut w = 0usize;
                        for &d in s {
                            if (unsafe { *bs_buf.get_unchecked((d as usize) >> 6) } >> (d & 63)) & 1
                                != 0
                            {
                                buf_a[w] = d;
                                w += 1;
                            }
                        }
                        sp_len = w;
                        in_bitset = false;
                    } else {
                        // Intersect bitset with large sparse slice
                        let mut new_bs = [0u64; BITSET_WORDS];
                        for &d in s {
                            let idx = (d as usize) >> 6;
                            let bit = 1u64 << (d & 63);
                            if (bs_buf[idx] & bit) != 0 {
                                new_bs[idx] |= bit;
                            }
                        }
                        bs_buf[..num_words].copy_from_slice(&new_bs[..num_words]);
                    }
                }
            }
        }
    }

    // Apply remaining operators strictly left-to-right
    for i in prefix_len..n_terms {
        let op = bq.ops[i - 1];
        let tref = get_term_ref(
            bq.terms[i],
            table,
            heavy_bitsets,
            num_words,
            csr_offsets,
            csr_data,
        );

        match op {
            BoolOp::And => {
                if !in_bitset {
                    if sp_len == 0 {
                        continue;
                    }
                    match tref {
                        TermRef::Heavy(hb) => {
                            let mut w = 0usize;
                            for r in 0..sp_len {
                                let d = buf_a[r];
                                if (unsafe { *hb.get_unchecked((d as usize) >> 6) } >> (d & 63))
                                    & 1
                                    != 0
                                {
                                    buf_a[w] = d;
                                    w += 1;
                                }
                            }
                            sp_len = w;
                        }
                        TermRef::Sparse(s) => {
                            let new_len = intersect_sorted_u16(&buf_a[..sp_len], s, &mut buf_b);
                            buf_a[..new_len].copy_from_slice(&buf_b[..new_len]);
                            sp_len = new_len;
                        }
                    }
                } else {
                    match tref {
                        TermRef::Heavy(hb) => {
                            for w in 0..num_words {
                                bs_buf[w] &= hb[w];
                            }
                        }
                        TermRef::Sparse(s) => {
                            if s.len() <= SPARSE_LIMIT {
                                let mut w = 0usize;
                                for &d in s {
                                    if (unsafe { *bs_buf.get_unchecked((d as usize) >> 6) }
                                        >> (d & 63))
                                        & 1
                                        != 0
                                    {
                                        buf_a[w] = d;
                                        w += 1;
                                    }
                                }
                                sp_len = w;
                                in_bitset = false;
                            } else {
                                let mut new_bs = [0u64; BITSET_WORDS];
                                for &d in s {
                                    let idx = (d as usize) >> 6;
                                    let bit = 1u64 << (d & 63);
                                    if (bs_buf[idx] & bit) != 0 {
                                        new_bs[idx] |= bit;
                                    }
                                }
                                bs_buf[..num_words].copy_from_slice(&new_bs[..num_words]);
                            }
                        }
                    }
                }
            }
            BoolOp::Not => {
                if !in_bitset {
                    if sp_len == 0 {
                        continue;
                    }
                    match tref {
                        TermRef::Heavy(hb) => {
                            let mut w = 0usize;
                            for r in 0..sp_len {
                                let d = buf_a[r];
                                if (unsafe { *hb.get_unchecked((d as usize) >> 6) } >> (d & 63))
                                    & 1
                                    == 0
                                {
                                    buf_a[w] = d;
                                    w += 1;
                                }
                            }
                            sp_len = w;
                        }
                        TermRef::Sparse(s) => {
                            let new_len = difference_sorted_u16(&buf_a[..sp_len], s, &mut buf_b);
                            buf_a[..new_len].copy_from_slice(&buf_b[..new_len]);
                            sp_len = new_len;
                        }
                    }
                } else {
                    match tref {
                        TermRef::Heavy(hb) => {
                            for w in 0..num_words {
                                bs_buf[w] &= !hb[w];
                            }
                        }
                        TermRef::Sparse(s) => {
                            for &d in s {
                                unsafe {
                                    *bs_buf.get_unchecked_mut((d as usize) >> 6) &=
                                        !(1u64 << (d & 63));
                                }
                            }
                        }
                    }
                }
            }
            BoolOp::Or => {
                if !in_bitset {
                    match tref {
                        TermRef::Sparse(s) if sp_len + s.len() <= SPARSE_LIMIT => {
                            let new_len = union_sorted_u16(&buf_a[..sp_len], s, &mut buf_b);
                            buf_a[..new_len].copy_from_slice(&buf_b[..new_len]);
                            sp_len = new_len;
                            continue;
                        }
                        _ => {
                            // Promote to bitset
                            for w in &mut bs_buf[..num_words] {
                                *w = 0;
                            }
                            for &d in &buf_a[..sp_len] {
                                unsafe {
                                    *bs_buf.get_unchecked_mut((d as usize) >> 6) |=
                                        1u64 << (d & 63);
                                }
                            }
                            in_bitset = true;
                        }
                    }
                }
                match tref {
                    TermRef::Heavy(hb) => {
                        for w in 0..num_words {
                            bs_buf[w] |= hb[w];
                        }
                    }
                    TermRef::Sparse(s) => {
                        for &d in s {
                            unsafe {
                                *bs_buf.get_unchecked_mut((d as usize) >> 6) |= 1u64 << (d & 63);
                            }
                        }
                    }
                }
            }
        }
    }

    // Accumulate into QueryResult
    if !in_bitset {
        res.count += sp_len as u32;
        let mut s_acc = 0u64;
        for &d in &buf_a[..sp_len] {
            let gid = block_start_doc + (d as u32);
            s_acc += gid as u64;
            if (res.top_len as usize) < 10 {
                res.top10[res.top_len as usize] = gid;
                res.top_len += 1;
            }
        }
        res.sum += s_acc;
    } else {
        let mut c_acc = 0u32;
        let mut s_acc = 0u64;
        for (w_idx, &word_val) in bs_buf[..num_words].iter().enumerate() {
            if word_val == 0 {
                continue;
            }
            let base = block_start_doc + ((w_idx as u32) << 6);
            let ones = word_val.count_ones();
            c_acc += ones;
            let mut w = word_val;
            while w != 0 {
                let tz = w.trailing_zeros();
                let gid = base + tz;
                s_acc += gid as u64;
                if (res.top_len as usize) < 10 {
                    res.top10[res.top_len as usize] = gid;
                    res.top_len += 1;
                }
                w &= w - 1;
            }
        }
        res.count += c_acc;
        res.sum += s_acc;
    }
}

fn eval_phrase_query_block(
    pq: &PhraseQuery,
    docs: &[u8],
    doc_offsets: &[u32],
    table: &TermTable,
    heavy_bitsets: &[u64],
    num_words: usize,
    csr_offsets: &[u32],
    csr_data: &[u16],
    block_start_doc: u32,
    res: &mut QueryResult,
    bs_buf: &mut [u64; BITSET_WORDS],
) {
    let n_filters = pq.num_filters as usize;
    let mut best_idx = 0usize;
    let mut best_ref = get_term_ref(
        pq.filter_terms[0],
        table,
        heavy_bitsets,
        num_words,
        csr_offsets,
        csr_data,
    );
    let mut best_len = best_ref.est_len();
    if best_len == 0 {
        return;
    }

    for i in 1..n_filters {
        let r = get_term_ref(
            pq.filter_terms[i],
            table,
            heavy_bitsets,
            num_words,
            csr_offsets,
            csr_data,
        );
        let l = r.est_len();
        if l == 0 {
            return;
        }
        if l < best_len {
            best_len = l;
            best_idx = i;
            best_ref = r;
        }
    }

    match best_ref {
        TermRef::Sparse(seed_slice) => {
            let mut cand_a = Vec::new();
            let mut cand_b = Vec::new();
            let mut current: &[u16] = seed_slice;

            for i in 0..n_filters {
                if i == best_idx {
                    continue;
                }
                if current.is_empty() {
                    return;
                }
                let tref = get_term_ref(
                    pq.filter_terms[i],
                    table,
                    heavy_bitsets,
                    num_words,
                    csr_offsets,
                    csr_data,
                );
                cand_b.clear();
                match tref {
                    TermRef::Heavy(hb) => {
                        for &d in current {
                            if (unsafe { *hb.get_unchecked((d as usize) >> 6) } >> (d & 63)) & 1
                                != 0
                            {
                                cand_b.push(d);
                            }
                        }
                    }
                    TermRef::Sparse(s) => {
                        let mut ia = 0usize;
                        let mut ib = 0usize;
                        while ia < current.len() && ib < s.len() {
                            let va = unsafe { *current.get_unchecked(ia) };
                            let vb = unsafe { *s.get_unchecked(ib) };
                            if va == vb {
                                cand_b.push(va);
                                ia += 1;
                                ib += 1;
                            } else if va < vb {
                                ia += 1;
                            } else {
                                ib += 1;
                            }
                        }
                    }
                }
                std::mem::swap(&mut cand_a, &mut cand_b);
                current = &cand_a;
            }

            for &d in current {
                let gid = (block_start_doc as usize) + (d as usize);
                if verify_phrase_in_doc(docs, doc_offsets, table, pq, gid) {
                    res.count += 1;
                    res.sum += gid as u64;
                    if (res.top_len as usize) < 10 {
                        res.top10[res.top_len as usize] = gid as u32;
                        res.top_len += 1;
                    }
                }
            }
        }
        TermRef::Heavy(hb) => {
            bs_buf[..num_words].copy_from_slice(hb);
            for i in 0..n_filters {
                if i == best_idx {
                    continue;
                }
                if let TermRef::Heavy(other_hb) = get_term_ref(
                    pq.filter_terms[i],
                    table,
                    heavy_bitsets,
                    num_words,
                    csr_offsets,
                    csr_data,
                ) {
                    for w in 0..num_words {
                        bs_buf[w] &= other_hb[w];
                    }
                }
            }
            for (w_idx, &word_val) in bs_buf[..num_words].iter().enumerate() {
                let mut w = word_val;
                let base = (block_start_doc as usize) + (w_idx << 6);
                while w != 0 {
                    let tz = w.trailing_zeros() as usize;
                    let gid = base + tz;
                    if verify_phrase_in_doc(docs, doc_offsets, table, pq, gid) {
                        res.count += 1;
                        res.sum += gid as u64;
                        if (res.top_len as usize) < 10 {
                            res.top10[res.top_len as usize] = gid as u32;
                            res.top_len += 1;
                        }
                    }
                    w &= w - 1;
                }
            }
        }
    }
}

#[inline(always)]
fn write_u64_ascii(mut val: u64, out: &mut Vec<u8>) {
    if val == 0 {
        out.push(b'0');
        return;
    }
    let mut buf = [0u8; 20];
    let mut i = 20;
    while val > 0 {
        i -= 1;
        buf[i] = b'0' + (val % 10) as u8;
        val /= 10;
    }
    out.extend_from_slice(&buf[i..20]);
}

struct SpinBarrier {
    step: AtomicUsize,
    arrived: AtomicUsize,
    total: usize,
}

impl SpinBarrier {
    fn new(total: usize) -> Self {
        Self {
            step: AtomicUsize::new(0),
            arrived: AtomicUsize::new(0),
            total,
        }
    }
    #[inline(always)]
    fn wait(&self) {
        let s = self.step.load(Ordering::Acquire);
        if self.arrived.fetch_add(1, Ordering::AcqRel) + 1 == self.total {
            self.arrived.store(0, Ordering::Release);
            self.step.fetch_add(1, Ordering::Release);
        } else {
            while self.step.load(Ordering::Acquire) == s {
                std::hint::spin_loop();
            }
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let (mut d_dir, mut o_dir) = ("/data".to_string(), "/out".to_string());
    while let Some(a) = args.next() {
        if a == "--data" {
            if let Some(p) = args.next() { d_dir = p; }
        } else if a == "--out" {
            if let Some(p) = args.next() { o_dir = p; }
        }
    }

    let docs_mmap = Mmap::open(&format!("{}/docs.txt", d_dir));
    let queries_mmap = Mmap::open(&format!("{}/queries.txt", d_dir));
    let docs = docs_mmap.as_slice();
    let queries_bytes = queries_mmap.as_slice();

    // 1. Parse queries.txt and intern all query terms
    let mut table = TermTable::new();
    let queries = parse_queries(queries_bytes, &mut table);
    let num_queries = queries.len();

    // 2. Parallel scan of docs.txt for line start offsets
    let mut thread_line_counts = [0usize; NUM_THREADS];
    std::thread::scope(|s| {
        let len = docs.len();
        for t in 0..NUM_THREADS {
            let b_start = (len * t) / NUM_THREADS;
            let b_end = (len * (t + 1)) / NUM_THREADS;
            let slice = &docs[b_start..b_end];
            let out_ref = unsafe { &mut *(&mut thread_line_counts[t] as *mut usize) };
            s.spawn(move || {
                let mut c = 0usize;
                for &b in slice {
                    if b == b'\n' {
                        c += 1;
                    }
                }
                *out_ref = c;
            });
        }
    });

    let num_docs: usize = thread_line_counts.iter().sum();
    let mut doc_offsets = vec![0u32; num_docs + 1];
    std::thread::scope(|s| {
        let len = docs.len();
        let mut prefix_lines = 1usize; // doc_offsets[0] = 0
        for t in 0..NUM_THREADS {
            let b_start = (len * t) / NUM_THREADS;
            let b_end = (len * (t + 1)) / NUM_THREADS;
            let slice = &docs[b_start..b_end];
            let write_start = prefix_lines;
            prefix_lines += thread_line_counts[t];
            let offsets_ptr = doc_offsets.as_mut_ptr() as usize;
            s.spawn(move || {
                let ptr = offsets_ptr as *mut u32;
                let mut idx = write_start;
                for (i, &b) in slice.iter().enumerate() {
                    if b == b'\n' {
                        unsafe {
                            *ptr.add(idx) = (b_start + i + 1) as u32;
                        }
                        idx += 1;
                    }
                }
            });
        }
    });

    // 3. Classify Heavy (Bitset) vs Sparse (CSR) terms
    classify_terms(docs, &doc_offsets, &mut table);
    let num_heavy = table.num_heavy;
    let num_sparse = table.num_sparse;

    // 4. Allocate reusable block index buffers (< 18 MB total)
    let mut heavy_bitsets = vec![0u64; num_heavy * BITSET_WORDS];
    let mut thread_counts: Vec<Vec<u16>> = (0..NUM_THREADS)
        .map(|_| vec![0u16; num_sparse])
        .collect();
    let mut thread_Offsets: Vec<Vec<u32>> = (0..NUM_THREADS)
        .map(|_| vec![0u32; num_sparse])
        .collect();
    let mut thread_pairs: Vec<Vec<u32>> = (0..NUM_THREADS)
        .map(|_| Vec::with_capacity(131072))
        .collect();
    let mut csr_offsets = vec![0u32; num_sparse + 1];
    let mut csr_data: Vec<u16> = Vec::with_capacity(1_500_000);
    let mut query_results = vec![QueryResult::new(); num_queries];

    let num_blocks = if num_docs == 0 {
        0
    } else {
        (num_docs + BLOCK_DOCS - 1) / BLOCK_DOCS
    };

    let barrier = Arc::new(SpinBarrier::new(NUM_THREADS));

    // 5. Persistent 12-thread execution across all 65,536-doc blocks
    let hb_addr = heavy_bitsets.as_mut_ptr() as usize;
    let tc_addr = thread_counts.as_mut_ptr() as usize;
    let to_addr = thread_Offsets.as_mut_ptr() as usize;
    let tp_addr = thread_pairs.as_mut_ptr() as usize;
    let co_addr = csr_offsets.as_mut_ptr() as usize;
    let cd_addr = &mut csr_data as *mut Vec<u16> as usize;
    let qr_addr = query_results.as_mut_ptr() as usize;

    std::thread::scope(|s| {
        let table_ref = &table;
        let queries_ref = &queries;
        let doc_offsets_ref = &doc_offsets;

        for tid in 0..NUM_THREADS {
            let bar = Arc::clone(&barrier);
            s.spawn(move || {
                let mut last_seen = vec![u16::MAX; num_sparse];
                let mut bs_scratch = [0u64; BITSET_WORDS];

                for b in 0..num_blocks {
                    let block_start = b * BLOCK_DOCS;
                    let block_docs = (num_docs - block_start).min(BLOCK_DOCS);
                    let block_words = (block_docs + 63) / 64;

                    // Partition words (64-doc aligned) across 12 threads
                    let w_start = (block_words * tid) / NUM_THREADS;
                    let w_end = (block_words * (tid + 1)) / NUM_THREADS;
                    let d_start = w_start * 64;
                    let d_end = (w_end * 64).min(block_docs);

                    let hb_ptr = hb_addr as *mut u64;
                    let my_counts = unsafe { &mut *(tc_addr as *mut Vec<u16>).add(tid) };
                    let my_pairs = unsafe { &mut *(tp_addr as *mut Vec<u32>).add(tid) };

                    // Clear thread slice of heavy bitsets & counts
                    for h in 0..num_heavy {
                        let base = h * BITSET_WORDS;
                        for w in w_start..w_end {
                            unsafe { *hb_ptr.add(base + w) = 0 };
                        }
                    }
                    my_counts.fill(0);
                    my_pairs.clear();
                    last_seen.fill(u16::MAX);

                    // Tokenize assigned documents in this block
                    for local_d in d_start..d_end {
                        let global_d = block_start + local_d;
                        let word_idx = local_d >> 6;
                        let bit_mask = 1u64 << (local_d & 63);
                        let doc_in_thread = (local_d - d_start) as u32;
                        let local_d_u16 = local_d as u16;

                        let text = doc_text_slice(docs, doc_offsets_ref, global_d);
                        let mut pos = 0usize;
                        let mut prev_tid = EMPTY_SLOT;

                        while let Some((next_pos, w0, w1, w2, w3, len)) =
                            scan_next_token(text, pos)
                        {
                            pos = next_pos;
                            if len > 32 {
                                prev_tid = EMPTY_SLOT;
                                continue;
                            }
                            let term_id = table_ref.lookup(w0, w1, w2, w3, len);
                            if term_id == EMPTY_SLOT {
                                prev_tid = EMPTY_SLOT;
                                continue;
                            }

                            // Check adjacent repeat (w w) before deduplicating term_id
                            if term_id == prev_tid {
                                let rep_id = unsafe {
                                    *table_ref.repeat_term.get_unchecked(term_id as usize)
                                };
                                if rep_id != u32::MAX {
                                    let rh = unsafe {
                                        *table_ref.is_heavy.get_unchecked(rep_id as usize)
                                    };
                                    if rh != u16::MAX {
                                        unsafe {
                                            *hb_ptr.add((rh as usize) * BITSET_WORDS + word_idx) |=
                                                bit_mask;
                                        }
                                    } else {
                                        let rs = unsafe {
                                            *table_ref.sparse_idx.get_unchecked(rep_id as usize)
                                                as usize
                                        };
                                        if unsafe { *last_seen.get_unchecked(rs) } != local_d_u16 {
                                            unsafe {
                                                *last_seen.get_unchecked_mut(rs) = local_d_u16;
                                                *my_counts.get_unchecked_mut(rs) += 1;
                                            }
                                            my_pairs.push(((rs as u32) << 13) | doc_in_thread);
                                        }
                                    }
                                }
                            }
                            prev_tid = term_id;

                            let h_slot = unsafe {
                                *table_ref.is_heavy.get_unchecked(term_id as usize)
                            };
                            if h_slot != u16::MAX {
                                unsafe {
                                    *hb_ptr.add((h_slot as usize) * BITSET_WORDS + word_idx) |=
                                        bit_mask;
                                }
                            } else {
                                let s_idx = unsafe {
                                    *table_ref.sparse_idx.get_unchecked(term_id as usize) as usize
                                };
                                if unsafe { *last_seen.get_unchecked(s_idx) } != local_d_u16 {
                                    unsafe {
                                        *last_seen.get_unchecked_mut(s_idx) = local_d_u16;
                                        *my_counts.get_unchecked_mut(s_idx) += 1;
                                    }
                                    my_pairs.push(((s_idx as u32) << 13) | doc_in_thread);
                                }
                            }
                        }
                    }

                    bar.wait();

                    // Compute prefix sums for CSR
                    if tid == 0 {
                        let co_slice = unsafe {
                            std::slice::from_raw_parts_mut(co_addr as *mut u32, num_sparse + 1)
                        };
                        let tc_slice = unsafe {
                            std::slice::from_raw_parts(tc_addr as *const Vec<u16>, NUM_THREADS)
                        };
                        let to_slice = unsafe {
                            std::slice::from_raw_parts_mut(to_addr as *mut Vec<u32>, NUM_THREADS)
                        };
                        let mut running = 0u32;
                        for s_idx in 0..num_sparse {
                            co_slice[s_idx] = running;
                            for t in 0..NUM_THREADS {
                                to_slice[t][s_idx] = running;
                                running += tc_slice[t][s_idx] as u32;
                            }
                        }
                        co_slice[num_sparse] = running;
                        let cd_vec = unsafe { &mut *(cd_addr as *mut Vec<u16>) };
                        if cd_vec.len() < running as usize {
                            cd_vec.resize(running as usize, 0);
                        }
                    }

                    bar.wait();

                    // Lock-free parallel scatter into CSR (automatically sorted ascending)
                    {
                        let my_offsets =
                            unsafe { &mut *(to_addr as *mut Vec<u32>).add(tid) };
                        let cd_ptr = unsafe { (*(cd_addr as *const Vec<u16>)).as_ptr() as *mut u16 };
                        let d_start_u16 = d_start as u16;
                        for &pair in my_pairs.iter() {
                            let s_idx = (pair >> 13) as usize;
                            let local_doc = ((pair & 0x1FFF) as u16) + d_start_u16;
                            let pos = unsafe { *my_offsets.get_unchecked(s_idx) as usize };
                            unsafe {
                                *cd_ptr.add(pos) = local_doc;
                                *my_offsets.get_unchecked_mut(s_idx) = (pos + 1) as u32;
                            }
                        }
                    }

                    bar.wait();

                    // Evaluate all 50,000 queries on this block in parallel
                    let hb_slice = unsafe {
                        std::slice::from_raw_parts(hb_addr as *const u64, num_heavy * BITSET_WORDS)
                    };
                    let co_slice = unsafe {
                        std::slice::from_raw_parts(co_addr as *const u32, num_sparse + 1)
                    };
                    let cd_slice = unsafe { (*(cd_addr as *const Vec<u16>)).as_slice() };
                    let qr_ptr = qr_addr as *mut QueryResult;

                    let q_start = (num_queries * tid) / NUM_THREADS;
                    let q_end = (num_queries * (tid + 1)) / NUM_THREADS;

                    for q_idx in q_start..q_end {
                        let res = unsafe { &mut *qr_ptr.add(q_idx) };
                        match unsafe { queries_ref.get_unchecked(q_idx) } {
                            Query::Bool(bq) => {
                                eval_bool_query_block(
                                    bq,
                                    table_ref,
                                    hb_slice,
                                    block_words,
                                    co_slice,
                                    cd_slice,
                                    block_start as u32,
                                    res,
                                    &mut bs_scratch,
                                );
                            }
                            Query::Phrase(pq) => {
                                eval_phrase_query_block(
                                    pq,
                                    docs,
                                    doc_offsets_ref,
                                    table_ref,
                                    hb_slice,
                                    block_words,
                                    co_slice,
                                    cd_slice,
                                    block_start as u32,
                                    res,
                                    &mut bs_scratch,
                                );
                            }
                        }
                    }

                    bar.wait();
                }
            });
        }
    });

    // 6. Format and write results.txt
    let _ = std::fs::create_dir_all(&o_dir);
    let mut out_buf = Vec::with_capacity(1_600_000);
    for r in &query_results {
        if r.count == 0 {
            out_buf.extend_from_slice(b"0 0\n");
        } else {
            write_u64_ascii(r.count as u64, &mut out_buf);
            out_buf.push(b' ');
            write_u64_ascii(r.sum, &mut out_buf);
            for i in 0..(r.top_len as usize) {
                out_buf.push(b' ');
                write_u64_ascii(r.top10[i] as u64, &mut out_buf);
            }
            out_buf.push(b'\n');
        }
    }

    let mut out_file = File::create(format!("{}/results.txt", o_dir)).expect("create results.txt failed");
    out_file.write_all(&out_buf).expect("write results.txt failed");
}
