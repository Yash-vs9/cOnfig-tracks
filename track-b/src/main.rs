#![allow(static_mut_refs)]
use std::fs::File;
use std::io::Write;
use std::os::unix::io::IntoRawFd;
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};

unsafe extern "C" {
    fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
    fn munmap(a: *mut u8, l: usize) -> i32;
    fn madvise(a: *mut u8, l: usize, adv: i32) -> i32;
    fn close(fd: i32) -> i32;
}

const H_SZ: usize = 131072;
const H_MSK: usize = H_SZ - 1;
const NUM_C: usize = 24;
const BS_TH: u32 = 768;
const MAX_W: usize = 512;

#[derive(Clone, Copy, Default)]
struct Slot { h32: u32, tid: u32 }
#[derive(Clone, Copy, Default)]
struct TInfo { w0: u64, w1: u64, off: u32, len: u16 }
#[derive(Clone, Copy)]
struct Query { kind: u8, k: u8, ops: u16, tids: [u32; 8] }
#[derive(Clone, Copy, Default)]
struct CRes { cnt: u32, sum: u64, top: [u32; 10], tc: u8 }

static mut CMAP: [u8; 256] = [0; 256];
static mut VTAB: [Slot; H_SZ] = [Slot { h32: 0, tid: 0 }; H_SZ];
static mut TINFO: [TInfo; 81920] = [TInfo { w0: 0, w1: 0, off: 0, len: 0 }; 81920];
static mut BLOOM: [u64; 8192] = [0; 8192];
static mut N_TERMS: usize = 0;

struct Arena {
    cnts: Vec<u32>, seen: Vec<u32>, off: Vec<u32>, cur_off: Vec<u32>, bs_id: Vec<u32>,
    doc_off: Vec<usize>, raw_post: Vec<u32>, bs: Vec<u64>, u16_p: Vec<u16>,
}

impl Arena {
    fn new(nt: usize, max_docs: usize) -> Self {
        Self {
            cnts: vec![0; nt], seen: vec![u32::MAX; nt], off: vec![0; nt], cur_off: vec![0; nt], bs_id: vec![u32::MAX; nt],
            doc_off: vec![0; max_docs + 1], raw_post: Vec::with_capacity(max_docs * 16), bs: Vec::new(), u16_p: Vec::new(),
        }
    }
}

#[inline(always)]
fn mix64(w0: u64, w1: u64, len: usize) -> u64 {
    let h = (w0 ^ (w1.wrapping_mul(0x9e3779b97f4a7c15).wrapping_add(len as u64))).wrapping_mul(0xbf58476d1ce4e5b9);
    h ^ (h >> 31)
}

#[inline(always)]
unsafe fn tail_eq(d_tail: &[u8], q_tail: &[u8]) -> bool {
    for i in 0..d_tail.len() {
        if *CMAP.get_unchecked(*d_tail.get_unchecked(i) as usize) != *q_tail.get_unchecked(i) {
            return false;
        }
    }
    true
}

#[inline(always)]
unsafe fn lookup_swar(w0: u64, w1: u64, d_buf: &[u8], tok_start: usize, len: usize, q_buf: &[u8], h: u64) -> u32 {
    let h32 = ((h ^ (h >> 32)) as u32) | 1;
    let mut idx = (h as usize) & H_MSK;
    while VTAB.get_unchecked(idx).h32 != 0 {
        let s = *VTAB.get_unchecked(idx);
        if s.h32 == h32 {
            let ti = *TINFO.get_unchecked(s.tid as usize);
            if ti.w0 == w0 && ti.w1 == w1 && ti.len as usize == len {
                if len <= 16 || tail_eq(&d_buf[tok_start + 16..tok_start + len], &q_buf[ti.off as usize + 16..ti.off as usize + len]) {
                    return s.tid;
                }
            }
        }
        idx = (idx + 1) & H_MSK;
    }
    u32::MAX
}

#[inline(always)]
unsafe fn read_tok(d: &[u8], p: &mut usize) -> (u64, u64, usize, usize) {
    let start = *p;
    let (mut w0, mut w1, mut len) = (0u64, 0u64, 0usize);
    let mut m = *CMAP.get_unchecked(*d.get_unchecked(*p) as usize);
    loop {
        if len < 8 { w0 |= (m as u64) << (len * 8); }
        else if len < 16 { w1 |= (m as u64) << ((len - 8) * 8); }
        len += 1; *p += 1;
        m = *CMAP.get_unchecked(*d.get_unchecked(*p) as usize);
        if m <= 1 { break; }
    }
    (w0, w1, start, len)
}

#[target_feature(enable = "avx2,bmi1,bmi2,lzcnt,popcnt")]
unsafe fn process_chunk(c: usize, d_buf: &[u8], q_buf: &[u8], queries: &[Query], rep_map: &[u32], doc_s: &[u32; NUM_C], doc_e: &[u32; NUM_C], c_byte: &[usize; NUM_C + 1], ar: &mut Arena, my_res: &mut [CRes]) {
    let (d0, c_docs) = (doc_s[c], (doc_e[c] - doc_s[c]) as usize);
    if c_docs == 0 { return; }
    let (nt, cw, r_sz) = (N_TERMS, (c_docs + 63) >> 6, rep_map.len());
    ar.cnts.fill(0); ar.seen.fill(u32::MAX); ar.raw_post.clear();

    let (mut p, pe) = (c_byte[c], c_byte[c + 1]);
    while p < pe {
        let mut d = 0u32;
        while *d_buf.get_unchecked(p) >= b'0' && *d_buf.get_unchecked(p) <= b'9' {
            d = d * 10 + (*d_buf.get_unchecked(p) - b'0') as u32; p += 1;
        }
        if *d_buf.get_unchecked(p) == b'\t' { p += 1; }
        let ld = (d - d0) as usize;
        *ar.doc_off.get_unchecked_mut(ld) = p;
        let (mut pos, mut p_tid, mut p_pos) = (0u32, u32::MAX, u32::MAX);

        loop {
            while *CMAP.get_unchecked(*d_buf.get_unchecked(p) as usize) == 0 { p += 1; }
            if *d_buf.get_unchecked(p) == b'\n' { p += 1; break; }
            let (w0, w1, ts, len) = read_tok(d_buf, &mut p);
            let h = mix64(w0, w1, len);
            let tid = if (*BLOOM.get_unchecked(((h >> 32) as usize) & 8191) >> (h & 63)) & 1 != 0 {
                lookup_swar(w0, w1, d_buf, ts, len, q_buf, h)
            } else { u32::MAX };
            if tid != u32::MAX {
                let t = tid as usize;
                if *ar.seen.get_unchecked(t) != d {
                    *ar.seen.get_unchecked_mut(t) = d;
                    *ar.cnts.get_unchecked_mut(t) += 1;
                    ar.raw_post.push((tid << 16) | (ld as u32));
                }
                if p_tid == tid && pos == p_pos + 1 && t < r_sz {
                    let rt = *rep_map.get_unchecked(t);
                    if rt != u32::MAX && *ar.seen.get_unchecked(rt as usize) != d {
                        *ar.seen.get_unchecked_mut(rt as usize) = d;
                        *ar.cnts.get_unchecked_mut(rt as usize) += 1;
                        ar.raw_post.push((rt << 16) | (ld as u32));
                    }
                }
                p_tid = tid; p_pos = pos;
            } else { p_tid = u32::MAX; }
            pos += 1;
        }
    }

    let (mut n_bs, mut tot_u16) = (0usize, 0u32);
    for i in 0..nt {
        ar.off[i] = tot_u16; ar.cur_off[i] = tot_u16;
        if ar.cnts[i] > BS_TH { ar.bs_id[i] = n_bs as u32; n_bs += 1; }
        else { ar.bs_id[i] = u32::MAX; tot_u16 += ar.cnts[i]; }
    }
    ar.bs.clear(); ar.bs.resize(n_bs * cw, 0);
    ar.u16_p.clear(); ar.u16_p.resize(tot_u16 as usize, 0);
    for &rp in &ar.raw_post {
        let (t, ld) = ((rp >> 16) as usize, (rp & 0xFFFF) as usize);
        let b = *ar.bs_id.get_unchecked(t);
        if b != u32::MAX { *ar.bs.get_unchecked_mut((b as usize) * cw + (ld >> 6)) |= 1u64 << (ld & 63); }
        else {
            let o = *ar.cur_off.get_unchecked(t);
            *ar.cur_off.get_unchecked_mut(t) = o + 1;
            *ar.u16_p.get_unchecked_mut(o as usize) = ld as u16;
        }
    }

    let (cnts, off, bs_id, bs, u16_p, doc_off) = (&ar.cnts, &ar.off, &ar.bs_id, &ar.bs, &ar.u16_p, &ar.doc_off);
    let get_bs = |tid: usize, out: &mut [u64; MAX_W]| {
        if cnts[tid] == 0 { out[..cw].fill(0); return; }
        if bs_id[tid] != u32::MAX {
            let s = (bs_id[tid] as usize) * cw;
            out[..cw].copy_from_slice(&bs[s..s + cw]);
        } else {
            out[..cw].fill(0);
            let s = off[tid] as usize;
            for &ld in &u16_p[s..s + cnts[tid] as usize] { out[(ld as usize) >> 6] |= 1u64 << (ld & 63); }
        }
    };

    let (mut clist, mut tmp, mut cand) = ([0u16; (BS_TH as usize) + 1], [0u16; (BS_TH as usize) + 1], [0u16; 32768]);
    let (mut curr, mut nb) = ([0u64; MAX_W], [0u64; MAX_W]);

    for (qi, q) in queries.iter().enumerate() {
        let r = &mut my_res[qi];
        if q.kind == 0 {
            let t0 = q.tids[0] as usize;
            let (mut clen, mut in_bs) = (cnts[t0] as usize, bs_id[t0] != u32::MAX);
            if in_bs { get_bs(t0, &mut curr); } else { let s = off[t0] as usize; clist[..clen].copy_from_slice(&u16_p[s..s + clen]); }
            for oi in 0..(q.k as usize - 1) {
                let (op, nt) = ((q.ops >> (oi * 2)) & 3, q.tids[oi + 1] as usize);
                let (ns, ne) = (off[nt] as usize, off[nt] as usize + cnts[nt] as usize);
                if in_bs {
                    if bs_id[nt] != u32::MAX {
                        let b = &bs[(bs_id[nt] as usize) * cw..][..cw];
                        if op == 1 { for w in 0..cw { curr[w] &= b[w]; } }
                        else if op == 2 { for w in 0..cw { curr[w] |= b[w]; } }
                        else { for w in 0..cw { curr[w] &= !b[w]; } }
                    } else if op == 2 { for &ld in &u16_p[ns..ne] { curr[(ld as usize) >> 6] |= 1u64 << (ld & 63); } }
                    else if op == 3 { for &ld in &u16_p[ns..ne] { curr[(ld as usize) >> 6] &= !(1u64 << (ld & 63)); } }
                    else { get_bs(nt, &mut nb); for w in 0..cw { curr[w] &= nb[w]; } }
                } else if op == 1 {
                    if clen == 0 { continue; }
                    let mut w = 0;
                    if bs_id[nt] != u32::MAX {
                        let b = &bs[(bs_id[nt] as usize) * cw..];
                        for i in 0..clen { let d = clist[i] as usize; if (b[d >> 6] >> (d & 63)) & 1 != 0 { clist[w] = clist[i]; w += 1; } }
                    } else {
                        let (mut i, mut j) = (0, ns);
                        while i < clen && j < ne {
                            if clist[i] == u16_p[j] { clist[w] = clist[i]; w += 1; i += 1; j += 1; }
                            else if clist[i] < u16_p[j] { i += 1; } else { j += 1; }
                        }
                    }
                    clen = w;
                } else if op == 3 {
                    if clen == 0 { continue; }
                    let mut w = 0;
                    if bs_id[nt] != u32::MAX {
                        let b = &bs[(bs_id[nt] as usize) * cw..];
                        for i in 0..clen { let d = clist[i] as usize; if (b[d >> 6] >> (d & 63)) & 1 == 0 { clist[w] = clist[i]; w += 1; } }
                    } else {
                        let (mut i, mut j) = (0, ns);
                        while i < clen && j < ne {
                            if clist[i] == u16_p[j] { i += 1; j += 1; }
                            else if clist[i] < u16_p[j] { clist[w] = clist[i]; w += 1; i += 1; } else { j += 1; }
                        }
                        while i < clen { clist[w] = clist[i]; w += 1; i += 1; }
                    }
                    clen = w;
                } else if bs_id[nt] != u32::MAX || clen + (ne - ns) > BS_TH as usize {
                    get_bs(nt, &mut curr);
                    for i in 0..clen { let d = clist[i] as usize; curr[d >> 6] |= 1u64 << (d & 63); }
                    in_bs = true;
                } else {
                    let (mut i, mut j, mut w) = (0, ns, 0);
                    while i < clen && j < ne {
                        if clist[i] == u16_p[j] { tmp[w] = clist[i]; w += 1; i += 1; j += 1; }
                        else if clist[i] < u16_p[j] { tmp[w] = clist[i]; w += 1; i += 1; } else { tmp[w] = u16_p[j]; w += 1; j += 1; }
                    }
                    while i < clen { tmp[w] = clist[i]; w += 1; i += 1; }
                    while j < ne { tmp[w] = u16_p[j]; w += 1; j += 1; }
                    clist[..w].copy_from_slice(&tmp[..w]); clen = w;
                }
            }
            if !in_bs {
                r.cnt = clen as u32;
                for i in 0..clen { let d = d0 + clist[i] as u32; r.sum += d as u64; if r.tc < 10 { r.top[r.tc as usize] = d; r.tc += 1; } }
            } else {
                if c_docs & 63 != 0 { curr[cw - 1] &= (1u64 << (c_docs & 63)) - 1; }
                for w in 0..cw {
                    let mut v = curr[w]; if v == 0 { continue; }
                    r.cnt += v.count_ones();
                    let base = d0 + ((w as u32) << 6);
                    while v != 0 {
                        let d = base + v.trailing_zeros(); r.sum += d as u64;
                        if r.tc < 10 { r.top[r.tc as usize] = d; r.tc += 1; }
                        v &= v - 1;
                    }
                }
            }
        } else {
            let (k, mut nd, mut dist, mut ok) = (q.k as usize, 0usize, [0usize; 16], true);
            for i in 0..k {
                let t = q.tids[i] as usize;
                if cnts[t] == 0 { ok = false; break; }
                if !dist[..nd].contains(&t) { dist[nd] = t; nd += 1; }
                if i + 1 < k && t == q.tids[i + 1] as usize && t < r_sz && rep_map[t] != u32::MAX {
                    let rt = rep_map[t] as usize;
                    if cnts[rt] == 0 { ok = false; break; }
                    if !dist[..nd].contains(&rt) { dist[nd] = rt; nd += 1; }
                }
            }
            if !ok { continue; }
            dist[..nd].sort_unstable_by_key(|&t| cnts[t]);
            let (rare, mut clen) = (dist[0], 0usize);
            if bs_id[rare] == u32::MAX {
                clen = cnts[rare] as usize;
                let s = off[rare] as usize;
                cand[..clen].copy_from_slice(&u16_p[s..s + clen]);
            } else {
                let src = &bs[(bs_id[rare] as usize) * cw..][..cw];
                for w in 0..cw {
                    let mut v = src[w];
                    while v != 0 { let id = (w << 6) + v.trailing_zeros() as usize; if id < c_docs { cand[clen] = id as u16; clen += 1; } v &= v - 1; }
                }
            }
            for &ot in &dist[1..nd] {
                if clen == 0 { break; }
                let (mut w, ns, ne) = (0usize, off[ot] as usize, off[ot] as usize + cnts[ot] as usize);
                if bs_id[ot] != u32::MAX {
                    let b = &bs[(bs_id[ot] as usize) * cw..];
                    for i in 0..clen { let d = cand[i] as usize; if (b[d >> 6] >> (d & 63)) & 1 != 0 { cand[w] = cand[i]; w += 1; } }
                } else {
                    let (mut i, mut j) = (0, ns);
                    while i < clen && j < ne {
                        if cand[i] == u16_p[j] { cand[w] = cand[i]; w += 1; i += 1; j += 1; }
                        else if cand[i] < u16_p[j] { i += 1; } else { j += 1; }
                    }
                }
                clen = w;
            }
            let mut pt = [TInfo::default(); 8];
            for i in 0..k { pt[i] = TINFO[q.tids[i] as usize]; }
            for ci in 0..clen {
                let ld = cand[ci] as usize;
                let (mut sp, mut m) = (doc_off[ld], false);
                loop {
                    while *CMAP.get_unchecked(*d_buf.get_unchecked(sp) as usize) == 0 { sp += 1; }
                    if *d_buf.get_unchecked(sp) == b'\n' { break; }
                    let (w0, w1, ts, l) = read_tok(d_buf, &mut sp);
                    if l == pt[0].len as usize && w0 == pt[0].w0 && w1 == pt[0].w1 && (l <= 16 || tail_eq(&d_buf[ts + 16..ts + l], &q_buf[pt[0].off as usize + 16..pt[0].off as usize + l])) {
                        let (mut xp, mut j) = (sp, 1usize);
                        while j < k {
                            while *CMAP.get_unchecked(*d_buf.get_unchecked(xp) as usize) == 0 { xp += 1; }
                            if *d_buf.get_unchecked(xp) == b'\n' { break; }
                            let (jw0, jw1, jts, jl) = read_tok(d_buf, &mut xp);
                            if jl != pt[j].len as usize || jw0 != pt[j].w0 || jw1 != pt[j].w1 || (jl > 16 && !tail_eq(&d_buf[jts + 16..jts + jl], &q_buf[pt[j].off as usize + 16..pt[j].off as usize + jl])) { break; }
                            j += 1;
                        }
                        if j == k { m = true; break; }
                    }
                }
                if m { let d = d0 + ld as u32; r.cnt += 1; r.sum += d as u64; if r.tc < 10 { r.top[r.tc as usize] = d; r.tc += 1; } }
            }
        }
    }
    let (ps, pe_pg) = (c_byte[c] & !4095, c_byte[c + 1] & !4095);
    if pe_pg > ps { madvise(d_buf.as_ptr().add(ps) as *mut u8, pe_pg - ps, 4); }
}

unsafe fn map_file(path: &str) -> &'static [u8] {
    let f = File::open(path).unwrap();
    let len = f.metadata().unwrap().len() as usize;
    let fd = f.into_raw_fd();
    let p = mmap(ptr::null_mut(), len, 1, 2, fd, 0);
    close(fd);
    std::slice::from_raw_parts(p, len)
}

fn push_u64(buf: &mut Vec<u8>, mut n: u64) {
    if n == 0 { buf.push(b'0'); return; }
    let (mut tmp, mut i) = ([0u8; 20], 20);
    while n > 0 { i -= 1; tmp[i] = b'0' + (n % 10) as u8; n /= 10; }
    buf.extend_from_slice(&tmp[i..]);
}
fn main() {
    unsafe {
        CMAP[b'\n' as usize] = 1;
        for c in b'0'..=b'9' { CMAP[c as usize] = c; }
        for c in b'a'..=b'z' { CMAP[c as usize] = c; CMAP[(c - 32) as usize] = c; }
        let (mut d_dir, mut o_dir) = ("/data".to_string(), "/out".to_string());
        let args: Vec<String> = std::env::args().collect();
        for w in args.chunks(2).skip(1) {
            if w.len() == 2 && w[0] == "--data" { d_dir = w[1].clone(); }
            else if w.len() == 2 && w[0] == "--out" { o_dir = w[1].clone(); }
        }
        let q_buf = map_file(&format!("{}/queries.txt", d_dir));
        let d_buf = map_file(&format!("{}/docs.txt", d_dir));

        let mut queries = Vec::with_capacity(16384);
        for mut line in q_buf.split(|&b| b == b'\n') {
            if line.ends_with(b"\r") { line = &line[..line.len() - 1]; }
            if line.len() < 6 { continue; }
            let ib = line.starts_with(b"BOOL ");
            let body = if ib { &line[5..] } else { &line[8..line.len() - 1] };
            let mut q = Query { kind: (!ib) as u8, k: 0, ops: 0, tids: [0; 8] };
            for tok in body.split(|&b| b == b' ').filter(|s| !s.is_empty()) {
                if ib && tok == b"AND" { q.ops |= 1 << ((q.k - 1) * 2); }
                else if ib && tok == b"OR" { q.ops |= 2 << ((q.k - 1) * 2); }
                else if ib && tok == b"NOT" { q.ops |= 3 << ((q.k - 1) * 2); }
                else {
                    let off = (tok.as_ptr() as usize - q_buf.as_ptr() as usize) as u32;
                    let (mut w0, mut w1, len) = (0u64, 0u64, tok.len());
                    ptr::copy_nonoverlapping(tok.as_ptr(), &mut w0 as *mut u64 as *mut u8, len.min(8));
                    if len > 8 { ptr::copy_nonoverlapping(tok.as_ptr().add(8), &mut w1 as *mut u64 as *mut u8, (len - 8).min(8)); }
                    let h = mix64(w0, w1, len);
                    let mut tid = lookup_swar(w0, w1, q_buf, off as usize, len, q_buf, h);
                    if tid == u32::MAX {
                        let mut idx = (h as usize) & H_MSK;
                        while VTAB[idx].h32 != 0 { idx = (idx + 1) & H_MSK; }
                        tid = N_TERMS as u32; N_TERMS += 1;
                        VTAB[idx] = Slot { h32: ((h ^ (h >> 32)) as u32) | 1, tid };
                        TINFO[tid as usize] = TInfo { w0, w1, off, len: len as u16 };
                        BLOOM[((h >> 32) as usize) & 8191] |= 1u64 << (h & 63);
                    }
                    q.tids[q.k as usize] = tid; q.k += 1;
                }
            }
            queries.push(q);
        }
        let mut rep_map = vec![u32::MAX; N_TERMS];
        for q in &queries {
            if q.kind == 1 {
                for i in 0..(q.k as usize - 1) {
                    let t = q.tids[i] as usize;
                    if t == q.tids[i + 1] as usize && rep_map[t] == u32::MAX {
                        let rt = N_TERMS as u32; N_TERMS += 1; rep_map[t] = rt; TINFO[rt as usize] = TINFO[t];
                    }
                }
            }
        }

        let f_sz = d_buf.len();
        let mut pl = f_sz - 1;
        while pl > 0 && d_buf[pl - 1] != b'\n' { pl -= 1; }
        let mut total_docs = 0u32;
        while pl < f_sz && d_buf[pl] >= b'0' && d_buf[pl] <= b'9' {
            total_docs = total_docs * 10 + (d_buf[pl] - b'0') as u32; pl += 1;
        }
        total_docs += 1;

        let dpc = (total_docs + NUM_C as u32 - 1) / NUM_C as u32;
        let (mut doc_s, mut doc_e, mut c_byte, mut max_cd) = ([0u32; NUM_C], [0u32; NUM_C], [0usize; NUM_C + 1], 0usize);
        c_byte[NUM_C] = f_sz;
        for c in 0..NUM_C {
            doc_s[c] = total_docs.min(if c > 0 { (c as u32 * dpc + 63) & !63 } else { 0 });
            if c > 0 { doc_e[c - 1] = doc_s[c]; max_cd = max_cd.max((doc_e[c - 1] - doc_s[c - 1]) as usize); }
        }
        doc_e[NUM_C - 1] = total_docs;
        max_cd = max_cd.max((doc_e[NUM_C - 1] - doc_s[NUM_C - 1]) as usize);

        for c in 1..NUM_C {
            let (mut lo, mut hi, mut ans) = (0isize, f_sz as isize - 1, f_sz);
            while lo <= hi {
                let m = (lo + hi) >> 1;
                let mut p = m as usize;
                while p < f_sz && p > 0 && d_buf[p - 1] != b'\n' { p += 1; }
                if p >= f_sz { hi = m - 1; continue; }
                let (mut id, mut cp) = (0u32, p);
                while cp < f_sz && d_buf[cp] >= b'0' && d_buf[cp] <= b'9' { id = id * 10 + (d_buf[cp] - b'0') as u32; cp += 1; }
                if id >= doc_s[c] { ans = p; hi = m - 1; } else { lo = p as isize + 1; }
            }
            c_byte[c] = ans;
        }

        let n_q = queries.len();
        let mut all_res = vec![CRes::default(); NUM_C * n_q];
        let res_ptr = all_res.as_mut_ptr() as usize;
        let next_c = AtomicUsize::new(0);

        std::thread::scope(|s| {
            for _ in 0..12 {
                let (nc, qr, rr, ds, de, cb) = (&next_c, &queries, &rep_map, &doc_s, &doc_e, &c_byte);
                s.spawn(move || {
                    let mut arena = Arena::new(N_TERMS, max_cd);
                    loop {
                        let c = nc.fetch_add(1, Ordering::Relaxed);
                        if c >= NUM_C { break; }
                        let slice = std::slice::from_raw_parts_mut((res_ptr as *mut CRes).add(c * n_q), n_q);
                        process_chunk(c, d_buf, q_buf, qr, rr, ds, de, cb, &mut arena, slice);
                    }
                });
            }
        });
        munmap(d_buf.as_ptr() as *mut u8, f_sz);
        munmap(q_buf.as_ptr() as *mut u8, q_buf.len());

        let mut wbuf = Vec::with_capacity(1 << 20);
        for qi in 0..n_q {
            let (mut cnt, mut sum, mut top, mut tc) = (0u64, 0u64, [0u32; 10], 0usize);
            for c in 0..NUM_C {
                let r = &all_res[c * n_q + qi];
                cnt += r.cnt as u64; sum += r.sum;
                for i in 0..(r.tc as usize) { if tc < 10 { top[tc] = r.top[i]; tc += 1; } }
            }
            if cnt == 0 { wbuf.extend_from_slice(b"0 0\n"); }
            else {
                push_u64(&mut wbuf, cnt); wbuf.push(b' '); push_u64(&mut wbuf, sum);
                for i in 0..tc { wbuf.push(b' '); push_u64(&mut wbuf, top[i] as u64); }
                wbuf.push(b'\n');
            }
        }
        File::create(format!("{}/results.txt", o_dir)).unwrap().write_all(&wbuf).unwrap();
    }
}