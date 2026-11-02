use std::{
    fs::File,
    io::Write,
    os::unix::io::IntoRawFd,
    ptr,
    sync::atomic::{AtomicUsize, Ordering},
};

extern "C" {
    fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
    fn _exit(s: i32) -> !;
}

const H_MULT: u64 = 0x517cc1b727220a95;

const CMAP: [u8; 256] = {
    let mut m = [0u8; 256];
    let mut i = 0u8;
    loop {
        if (i >= b'0' && i <= b'9') || (i >= b'a' && i <= b'z') {
            m[i as usize] = i;
        } else if i >= b'A' && i <= b'Z' {
            m[i as usize] = i + 32;
        }
        if i == 255 {
            break;
        }
        i += 1;
    }
    m
};

#[inline(always)]
fn token_hash(s: &[u8]) -> u64 {
    let mut h = 0u64;
    for &b in s {
        let c = unsafe { *CMAP.get_unchecked(b as usize) };
        h = (h ^ (c as u64)).wrapping_mul(H_MULT);
    }
    h ^ (s.len() as u64).wrapping_mul(0x9e3779b97f4a7c15)
}

struct Table {
    keys: Vec<u64>,
    ids: Vec<u32>,
}

impl Table {
    fn new(cap: usize) -> Self {
        Self {
            keys: vec![0; cap],
            ids: vec![u32::MAX; cap],
        }
    }
    #[inline(always)]
    fn lookup(&self, mut key: u64) -> Option<u32> {
        if key == 0 {
            key = 1;
        }
        let mask = self.keys.len() - 1;
        let mut idx = (key.wrapping_mul(0x9e3779b97f4a7c15) as usize) & mask;
        loop {
            let k = *unsafe { self.keys.get_unchecked(idx) };
            if k == key {
                return Some(*unsafe { self.ids.get_unchecked(idx) });
            }
            if k == 0 {
                return None;
            }
            idx = (idx + 1) & mask;
        }
    }
    fn insert(&mut self, mut key: u64, id: u32) {
        if key == 0 {
            key = 1;
        }
        let mask = self.keys.len() - 1;
        let mut idx = (key.wrapping_mul(0x9e3779b97f4a7c15) as usize) & mask;
        while self.keys[idx] != 0 && self.keys[idx] != key {
            idx = (idx + 1) & mask;
        }
        self.keys[idx] = key;
        self.ids[idx] = id;
    }
}

enum Query {
    Bool {
        ops: Vec<u8>,
        terms: Vec<u32>,
    },
    Phrase {
        terms: Vec<u32>,
        words: Vec<Vec<u8>>,
    },
}

#[inline(always)]
fn id_prefix_len(d: u32) -> usize {
    match d {
        0..=9 => 2,
        10..=99 => 3,
        100..=999 => 4,
        1000..=9999 => 5,
        10000..=99999 => 6,
        _ => 7,
    }
}

#[inline(always)]
fn push_u64(buf: &mut Vec<u8>, mut n: u64) {
    let (mut tmp, mut i) = ([0u8; 20], 20);
    loop {
        i -= 1;
        tmp[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    buf.extend_from_slice(&tmp[i..]);
}

#[inline(always)]
fn fmt_res(cnt: u64, sum: u64, top: &[u32], out: &mut Vec<u8>) {
    if cnt == 0 {
        out.extend_from_slice(b"0 0\n");
    } else {
        push_u64(out, cnt);
        out.push(b' ');
        push_u64(out, sum);
        for &d in top {
            out.push(b' ');
            push_u64(out, d as u64);
        }
        out.push(b'\n');
    }
}

unsafe fn map_file(path: &str) -> &'static [u8] {
    let f = File::open(path).unwrap();
    let len = f.metadata().unwrap().len() as usize;
    let fd = f.into_raw_fd();
    let p = mmap(ptr::null_mut(), len, 1, 1, fd, 0);
    std::slice::from_raw_parts(p, len)
}

struct Shard {
    offsets: Vec<u32>,
    docs: Vec<u16>,
}

impl Shard {
    #[inline(always)]
    fn postings(&self, id: u32) -> &[u16] {
        let i = id as usize;
        let s = self.offsets[i] as usize;
        let e = self.offsets[i + 1] as usize;
        &self.docs[s..e]
    }
}

fn intersect(a: &[u16], b: &[u16], out: &mut Vec<u16>) {
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

#[inline(always)]
fn scan_tokens<F: FnMut(usize)>(text: &[u8], table: &Table, mut f: F) {
    let (mut i, n) = (0, text.len());
    while i < n {
        while i < n && unsafe { *CMAP.get_unchecked(*text.get_unchecked(i) as usize) } == 0 {
            i += 1;
        }
        if i >= n {
            break;
        }
        let st = i;
        let mut h = 0u64;
        while i < n {
            let c = unsafe { *CMAP.get_unchecked(*text.get_unchecked(i) as usize) };
            if c == 0 {
                break;
            }
            h = (h ^ (c as u64)).wrapping_mul(H_MULT);
            i += 1;
        }
        let th = h ^ ((i - st) as u64).wrapping_mul(0x9e3779b97f4a7c15);
        if let Some(tid) = table.lookup(th) {
            f(tid as usize);
        }
    }
}

#[inline(always)]
fn doc_has_phrase(text: &[u8], words: &[Vec<u8>]) -> bool {
    let (mut i, n, mut mi) = (0, text.len(), 0);
    while i < n {
        while i < n && unsafe { *CMAP.get_unchecked(*text.get_unchecked(i) as usize) } == 0 {
            i += 1;
        }
        if i >= n {
            break;
        }
        let st = i;
        while i < n && unsafe { *CMAP.get_unchecked(*text.get_unchecked(i) as usize) } != 0 {
            i += 1;
        }
        let tok = &text[st..i];
        if tok.eq_ignore_ascii_case(&words[mi]) {
            mi += 1;
            if mi == words.len() {
                return true;
            }
        } else if tok.eq_ignore_ascii_case(&words[0]) {
            mi = 1;
        } else {
            mi = 0;
        }
    }
    false
}

fn main() {
    let mut args = std::env::args().skip(1);
    let (mut d_dir, mut o_dir) = ("/data".into(), "/out".into());
    while let Some(a) = args.next() {
        if a == "--data" {
            d_dir = args.next().unwrap();
        } else if a == "--out" {
            o_dir = args.next().unwrap();
        }
    }
    let d_buf = unsafe { map_file(&format!("{}/docs.txt", d_dir)) };
    let q_buf = unsafe { map_file(&format!("{}/queries.txt", d_dir)) };

    let mut table = Table::new(131072);
    let mut term_count = 0u32;
    let get_id = |th: u64, t: &mut Table, tc: &mut u32| -> u32 {
        if let Some(id) = t.lookup(th) {
            id
        } else {
            let id = *tc;
            *tc += 1;
            t.insert(th, id);
            id
        }
    };

    let mut queries = Vec::with_capacity(50000);
    for line in std::str::from_utf8(q_buf).unwrap().lines() {
        if let Some(rest) = line.strip_prefix("BOOL ") {
            let (mut terms, mut ops) = (Vec::new(), Vec::new());
            for part in rest.split_ascii_whitespace() {
                match part {
                    "AND" => ops.push(1),
                    "OR" => ops.push(2),
                    "NOT" => ops.push(3),
                    w => terms.push(get_id(
                        token_hash(w.as_bytes()),
                        &mut table,
                        &mut term_count,
                    )),
                }
            }
            queries.push(Query::Bool { ops, terms });
        } else if let Some(rest) = line.strip_prefix("PHRASE \"") {
            let p = rest.strip_suffix('"').unwrap_or(rest);
            let (mut terms, mut words) = (Vec::new(), Vec::new());
            for w in p.split_ascii_whitespace() {
                terms.push(get_id(
                    token_hash(w.as_bytes()),
                    &mut table,
                    &mut term_count,
                ));
                words.push(w.as_bytes().to_vec());
            }
            queries.push(Query::Phrase { terms, words });
        }
    }

    let mut doc_starts = Vec::with_capacity(250001);
    doc_starts.push(0u32);
    for (i, &b) in d_buf.iter().enumerate() {
        if b == b'\n' && i + 1 < d_buf.len() {
            doc_starts.push((i + 1) as u32);
        }
    }
    let total_docs = doc_starts.len();
    doc_starts.push(d_buf.len() as u32);

    let (num_shards, n_terms) = (12.min(total_docs), term_count as usize);
    let mut shards: Vec<Shard> = (0..num_shards)
        .map(|_| Shard {
            offsets: Vec::new(),
            docs: Vec::new(),
        })
        .collect();

    std::thread::scope(|s| {
        for (si, shard) in shards.iter_mut().enumerate() {
            let (table, doc_starts) = (&table, &doc_starts);
            s.spawn(move || {
                let (d_start, d_end) = (
                    si * total_docs / num_shards,
                    (si + 1) * total_docs / num_shards,
                );
                let (mut cnt, mut last_seen) = (vec![0u16; n_terms], vec![0u16; n_terms]);

                // Pass 1: count
                for (ld_idx, d) in (d_start..d_end).enumerate() {
                    let ld = (ld_idx + 1) as u16;
                    let text = &d_buf[doc_starts[d] as usize + id_prefix_len(d as u32)
                        ..doc_starts[d + 1] as usize];
                    scan_tokens(text, table, |ti| {
                        if last_seen[ti] != ld {
                            last_seen[ti] = ld;
                            cnt[ti] += 1;
                        }
                    });
                }

                let mut offsets = Vec::with_capacity(n_terms + 1);
                let mut tot = 0u32;
                for &c in &cnt {
                    offsets.push(tot);
                    tot += c as u32;
                }
                offsets.push(tot);

                let mut docs = vec![0u16; tot as usize];
                let mut wp = offsets.clone();
                last_seen.fill(0);

                // Pass 2: fill
                for (ld_idx, d) in (d_start..d_end).enumerate() {
                    let (ld_u16, ld) = (ld_idx as u16, (ld_idx + 1) as u16);
                    let text = &d_buf[doc_starts[d] as usize + id_prefix_len(d as u32)
                        ..doc_starts[d + 1] as usize];
                    scan_tokens(text, table, |ti| {
                        if last_seen[ti] != ld {
                            last_seen[ti] = ld;
                            docs[wp[ti] as usize] = ld_u16;
                            wp[ti] += 1;
                        }
                    });
                }
                *shard = Shard { offsets, docs };
            });
        }
    });

    const BATCH: usize = 256;
    let num_batches = (queries.len() + BATCH - 1) / BATCH;
    let out_chunks: Vec<Vec<u8>> = (0..num_batches)
        .map(|_| Vec::with_capacity(BATCH * 32))
        .collect();
    let next_batch = AtomicUsize::new(0);

    std::thread::scope(|s| {
        for _ in 0..12 {
            let (shards, queries, doc_starts, next_batch) =
                (&shards, &queries, &doc_starts, &next_batch);
            let out_ptr = out_chunks.as_ptr() as usize;
            s.spawn(move || {
                let (mut c1, mut c2) = (Vec::new(), Vec::new());
                loop {
                    let b = next_batch.fetch_add(1, Ordering::Relaxed);
                    if b >= num_batches {
                        break;
                    }
                    let out = unsafe { &mut *(out_ptr as *mut Vec<u8>).add(b) };
                    for q in &queries[b * BATCH..(b * BATCH + BATCH).min(queries.len())] {
                        let (mut cnt, mut sum, mut top) = (0u64, 0u64, Vec::new());
                        match q {
                            Query::Phrase { terms, words } => {
                                for (si, shard) in shards.iter().enumerate() {
                                    let d_base = (si * total_docs / num_shards) as u32;
                                    let mut rarest = usize::MAX;
                                    let mut min_l = usize::MAX;
                                    for &id in terms {
                                        let l = shard.postings(id).len();
                                        if l < min_l {
                                            min_l = l;
                                            rarest = id as usize;
                                        }
                                    }
                                    if min_l == 0 {
                                        continue;
                                    }
                                    c1.clear();
                                    c1.extend_from_slice(shard.postings(rarest as u32));
                                    for &id in terms {
                                        if id as usize != rarest {
                                            intersect(&c1, shard.postings(id), &mut c2);
                                            std::mem::swap(&mut c1, &mut c2);
                                            if c1.is_empty() {
                                                break;
                                            }
                                        }
                                    }
                                    for &ld in &c1 {
                                        let d = d_base + ld as u32;
                                        let text = &d_buf[doc_starts[d as usize] as usize
                                            + id_prefix_len(d)
                                            ..doc_starts[d as usize + 1] as usize];
                                        if doc_has_phrase(text, words) {
                                            cnt += 1;
                                            sum += d as u64;
                                            if top.len() < 10 {
                                                top.push(d);
                                            }
                                        }
                                    }
                                }
                            }
                            Query::Bool { ops, terms } => {
                                let is_pure_and = ops.iter().all(|&op| op == 1);
                                for (si, shard) in shards.iter().enumerate() {
                                    let d_base = (si * total_docs / num_shards) as u32;
                                    let d_len = (si + 1) * total_docs / num_shards
                                        - si * total_docs / num_shards;
                                    if is_pure_and {
                                        c1.clear();
                                        c1.extend_from_slice(shard.postings(terms[0]));
                                        for &t in &terms[1..] {
                                            intersect(&c1, shard.postings(t), &mut c2);
                                            std::mem::swap(&mut c1, &mut c2);
                                            if c1.is_empty() {
                                                break;
                                            }
                                        }
                                        for &ld in &c1 {
                                            let d = d_base + ld as u32;
                                            cnt += 1;
                                            sum += d as u64;
                                            if top.len() < 10 {
                                                top.push(d);
                                            }
                                        }
                                    } else {
                                        let nw = (d_len + 63) / 64;
                                        let mut bs = [0u64; 330];
                                        for &ld in shard.postings(terms[0]) {
                                            bs[(ld as usize) >> 6] |= 1 << (ld & 63);
                                        }
                                        for (i, &op) in ops.iter().enumerate() {
                                            let post = shard.postings(terms[i + 1]);
                                            match op {
                                                1 => {
                                                    let mut nbs = [0u64; 330];
                                                    for &ld in post {
                                                        nbs[(ld as usize) >> 6] |= 1 << (ld & 63);
                                                    }
                                                    for w in 0..nw {
                                                        bs[w] &= nbs[w];
                                                    }
                                                }
                                                2 => {
                                                    for &ld in post {
                                                        bs[(ld as usize) >> 6] |= 1 << (ld & 63);
                                                    }
                                                }
                                                3 => {
                                                    for &ld in post {
                                                        bs[(ld as usize) >> 6] &= !(1 << (ld & 63));
                                                    }
                                                }
                                                _ => {}
                                            }
                                        }
                                        for w in 0..nw {
                                            let mut val = bs[w];
                                            if w == nw - 1 && (d_len & 63) != 0 {
                                                val &= (1u64 << (d_len & 63)) - 1;
                                            }
                                            let base = d_base + (w << 6) as u32;
                                            while val != 0 {
                                                let d = base + val.trailing_zeros();
                                                cnt += 1;
                                                sum += d as u64;
                                                if top.len() < 10 {
                                                    top.push(d);
                                                }
                                                val &= val - 1;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        fmt_res(cnt, sum, &top, out);
                    }
                }
            });
        }
    });

    let _ = std::fs::create_dir_all(&o_dir);
    let mut f = File::create(format!("{}/results.txt", o_dir)).unwrap();
    for c in &out_chunks {
        f.write_all(c).unwrap();
    }
    unsafe {
        _exit(0);
    }
}
