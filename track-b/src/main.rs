use std::arch::x86_64::*;
use std::fs::File;
use std::io::Write;
use std::os::unix::io::IntoRawFd;
use std::ptr;

extern "C" {
    fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
    fn munmap(a: *mut u8, l: usize) -> i32;
    fn madvise(a: *mut u8, l: usize, adv: i32) -> i32;
    fn close(fd: i32) -> i32;
}

const H_SZ: usize = 131072;
const H_MSK: usize = H_SZ - 1;
const NUM_C: usize = 12;
const BS_TH: u32 = 1024;
const MAX_W: usize = 512;

#[derive(Clone, Copy, Default)]
struct Slot { h32: u32, tid: u32 }
#[derive(Clone, Copy, Default)]
struct TInfo { w0: u64, w1: u64, off: u32, len: u16 }
#[derive(Clone, Copy)]
struct Query { kind: u8, k: u8, ops: u16, tids: [u32; 8] }
#[derive(Clone, Copy, Default)]
struct CRes { cnt: u32, sum: u64, top: [u32; 10], t_cnt: u8 }

static mut CMAP: [u8; 256] = [0; 256];
static mut VTAB: [Slot; H_SZ] = [Slot { h32: 0, tid: 0 }; H_SZ];
static mut TINFO: [TInfo; 81920] = [TInfo { w0: 0, w1: 0, off: 0, len: 0 }; 81920];
static mut BLOOM: [u64; 8192] = [0; 8192];
static mut N_TERMS: usize = 0;

#[inline(always)]
fn mix64(w0: u64, w1: u64, len: usize) -> u64 {
    let h = (w0 ^ (w1.wrapping_mul(0x9e3779b97f4a7c15).wrapping_add(len as u64))).wrapping_mul(0xbf58476d1ce4e5b9);
    h ^ (h >> 31)
}

#[inline(always)]
unsafe fn lookup_swar(w0: u64, w1: u64, tail: &[u8], len: usize, q_buf: &[u8], h: u64) -> u32 {
    let h32 = ((h ^ (h >> 32)) as u32) | 1;
    let mut idx = (h as usize) & H_MSK;
    while VTAB.get_unchecked(idx).h32 != 0 {
        let s = *VTAB.get_unchecked(idx);
        if s.h32 == h32 {
            let ti = *TINFO.get_unchecked(s.tid as usize);
            if ti.w0 == w0 && ti.w1 == w1 && ti.len as usize == len {
                if len <= 16 || &q_buf[ti.off as usize + 16..ti.off as usize + len] == &tail[..len - 16] {
                    return s.tid;
                }
            }
        }
        idx = (idx + 1) & H_MSK;
    }
    u32::MAX
}

unsafe fn insert_term(s: &[u8], off: u32, q_buf: &[u8]) -> u32 {
    let len = s.len();
    let (mut w0, mut w1) = (0u64, 0u64);
    ptr::copy_nonoverlapping(s.as_ptr(), &mut w0 as *mut u64 as *mut u8, len.min(8));
    if len > 8 { ptr::copy_nonoverlapping(s.as_ptr().add(8), &mut w1 as *mut u64 as *mut u8, (len - 8).min(8)); }
    let h = mix64(w0, w1, len);
    let tail = if len > 16 { &s[16..] } else { &[] };
    let tid = lookup_swar(w0, w1, tail, len, q_buf, h);
    if tid != u32::MAX { return tid; }
    let h32 = ((h ^ (h >> 32)) as u32) | 1;
    let mut idx = (h as usize) & H_MSK;
    while VTAB[idx].h32 != 0 { idx = (idx + 1) & H_MSK; }
    let nt = N_TERMS as u32;
    N_TERMS += 1;
    VTAB[idx] = Slot { h32, tid: nt };
    TINFO[nt as usize] = TInfo { w0, w1, off, len: len as u16 };
    BLOOM[((h >> 32) as usize) & 8191] |= 1u64 << (h & 63);
    nt
}

#[inline(always)]
unsafe fn read_tok(d: &[u8], p: &mut usize, tail: &mut [u8; 32]) -> (u64, u64, usize) {
    let (mut w0, mut w1, mut len) = (0u64, 0u64, 0usize);
    let mut m = *CMAP.get_unchecked(*d.get_unchecked(*p) as usize);
    loop {
        if len < 8 { w0 |= (m as u64) << (len * 8); }
        else if len < 16 { w1 |= (m as u64) << ((len - 8) * 8); }
        else if len < 48 { *tail.get_unchecked_mut(len - 16) = m; }
        len += 1; *p += 1;
        m = *CMAP.get_unchecked(*d.get_unchecked(*p) as usize);
        if m <= 1 { break; }
    }
    (w0, w1, len)
}

#[target_feature(enable = "avx2,bmi1,bmi2,lzcnt,popcnt")]
unsafe fn process_chunk(c: usize, d_buf: &[u8], q_buf: &[u8], queries: &[Query], rep_map: &[u32], doc_s: &[u32; NUM_C], doc_e: &[u32; NUM_C], c_byte: &[usize; NUM_C + 1], out_res: &mut [CRes]) {
    let (d0, c_docs) = (doc_s[c], (doc_e[c] - doc_s[c]) as usize);
    if c_docs == 0 { return; }
    let (nt, c_words, r_sz) = (N_TERMS, (c_docs + 63) >> 6, rep_map.len());
    let mut cnts = vec![0u32; nt];
    let mut seen = vec![u32::MAX; nt];
    let mut doc_off = vec![0u32; c_docs];
    let mut pairs = Vec::<u64>::with_capacity(c_docs * 16);
    let (mut p, pe) = (c_byte[c], c_byte[c + 1]);

    while p < pe {
        let mut d = 0u32;
        while *d_buf.get_unchecked(p) >= b'0' && *d_buf.get_unchecked(p) <= b'9' { d = d * 10 + (*d_buf.get_unchecked(p) - b'0') as u32; p += 1; }
        if *d_buf.get_unchecked(p) == b'\t' { p += 1; }
        let ld = (d - d0) as usize;
        *doc_off.get_unchecked_mut(ld) = p as u32;
        let (mut pos, mut p_tid, mut p_pos) = (0u32, u32::MAX, u32::MAX);
        loop {
            while *CMAP.get_unchecked(*d_buf.get_unchecked(p) as usize) == 0 { p += 1; }
            if *d_buf.get_unchecked(p) == b'\n' { p += 1; break; }
            let mut tail = [0u8; 32];
            let (w0, w1, len) = read_tok(d_buf, &mut p, &mut tail);
            let h = mix64(w0, w1, len);
            let tid = if (*BLOOM.get_unchecked(((h >> 32) as usize) & 8191) >> (h & 63)) & 1 != 0 { lookup_swar(w0, w1, &tail, len, q_buf, h) } else { u32::MAX };
            if tid != u32::MAX {
                let t = tid as usize;
                if *seen.get_unchecked(t) != d { *seen.get_unchecked_mut(t) = d; *cnts.get_unchecked_mut(t) += 1; pairs.push(((tid as u64) << 16) | (ld as u64)); }
                if p_tid == tid && pos == p_pos + 1 && t < r_sz {
                    let rt = *rep_map.get_unchecked(t);
                    if rt != u32::MAX && *seen.get_unchecked(rt as usize) != d {
                        *seen.get_unchecked_mut(rt as usize) = d; *cnts.get_unchecked_mut(rt as usize) += 1; pairs.push(((rt as u64) << 16) | (ld as u64));
                    }
                }
                p_tid = tid; p_pos = pos;
            } else { p_tid = u32::MAX; }
            pos += 1;
        }
    }
    drop(seen);

    let (mut off, mut bs_id, mut n_bs, mut tot_u16) = (vec![0u32; nt], vec![u32::MAX; nt], 0usize, 0u32);
    for i in 0..nt {
        off[i] = tot_u16;
        if cnts[i] > BS_TH { bs_id[i] = n_bs as u32; n_bs += 1; } else { tot_u16 += cnts[i]; }
    }
    let mut bs = vec![0u64; n_bs * c_words];
    let mut u16_p = vec![0u16; tot_u16 as usize];
    let mut cur_off = off.clone();
    for &pair in &pairs {
        let (t, ld) = ((pair >> 16) as usize, (pair & 0xFFFF) as usize);
        let b_id = *bs_id.get_unchecked(t);
        if b_id != u32::MAX { *bs.get_unchecked_mut((b_id as usize) * c_words + (ld >> 6)) |= 1u64 << (ld & 63); }
        else { let o = *cur_off.get_unchecked(t); *cur_off.get_unchecked_mut(t) = o + 1; *u16_p.get_unchecked_mut(o as usize) = ld as u16; }
    }
    drop(pairs); drop(cur_off);

    let get_bs = |tid: usize, out: &mut [u64; MAX_W]| {
        if cnts[tid] == 0 { out[..c_words].fill(0); return; }
        if bs_id[tid] != u32::MAX {
            let s = (bs_id[tid] as usize) * c_words;
            out[..c_words].copy_from_slice(&bs[s..s + c_words]);
        } else {
            out[..c_words].fill(0);
            let s = off[tid] as usize;
            for &ld in &u16_p[s..s + cnts[tid] as usize] { out[(ld as usize) >> 6] |= 1u64 << (ld & 63); }
        }
    };

    let (mut clist, mut tmp, mut cand) = ([0u16; 1025], [0u16; 1025], [0u16; 25000]);
    let (mut curr, mut nb) = ([0u64; MAX_W], [0u64; MAX_W]);

    for (qi, q) in queries.iter().enumerate() {
        let r = &mut out_res[qi];
        if q.kind == 0 {
            let t0 = q.tids[0] as usize;
            let mut clen = cnts[t0] as usize;
            let mut in_bs = bs_id[t0] != u32::MAX;
            if in_bs { get_bs(t0, &mut curr); } else { let s = off[t0] as usize; clist[..clen].copy_from_slice(&u16_p[s..s + clen]); }
            for oi in 0..(q.k as usize - 1) {
                let (op, nt) = ((q.ops >> (oi * 2)) & 3, q.tids[oi + 1] as usize);
                let (ns, ne) = (off[nt] as usize, off[nt] as usize + cnts[nt] as usize);
                if in_bs {
                    if bs_id[nt] != u32::MAX {
                        let b = &bs[(bs_id[nt] as usize) * c_words..][..c_words];
                        if op == 1 { for w in 0..c_words { curr[w] &= b[w]; } }
                        else if op == 2 { for w in 0..c_words { curr[w] |= b[w]; } }
                        else { for w in 0..c_words { curr[w] &= !b[w]; } }
                    } else if op == 2 { for &ld in &u16_p[ns..ne] { curr[(ld as usize) >> 6] |= 1u64 << (ld & 63); } }
                    else if op == 3 { for &ld in &u16_p[ns..ne] { curr[(ld as usize) >> 6] &= !(1u64 << (ld & 63)); } }
                    else { get_bs(nt, &mut nb); for w in 0..c_words { curr[w] &= nb[w]; } }
                } else if op == 1 {
                    if clen == 0 { continue; }
                    let mut w = 0;
                    if bs_id[nt] != u32::MAX {
                        let b = &bs[(bs_id[nt] as usize) * c_words..];
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
                        let b = &bs[(bs_id[nt] as usize) * c_words..];
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
                } else {
                    if bs_id[nt] != u32::MAX || clen + (ne - ns) > BS_TH as usize {
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
            }
            if !in_bs {
                r.cnt = clen as u32;
                for i in 0..clen { let d = d0 + clist[i] as u32; r.sum += d as u64; if r.t_cnt < 10 { r.top[r.t_cnt as usize] = d; r.t_cnt += 1; } }
            } else {
                if c_docs & 63 != 0 { curr[c_words - 1] &= (1u64 << (c_docs & 63)) - 1; }
                for w in 0..c_words {
                    let mut v = curr[w]; if v == 0 { continue; }
                    r.cnt += v.count_ones();
                    let base = d0 + ((w as u32) << 6);
                    while v != 0 {
                        let d = base + v.trailing_zeros(); r.sum += d as u64;
                        if r.t_cnt < 10 { r.top[r.t_cnt as usize] = d; r.t_cnt += 1; }
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
            let rare = dist[0];
            let mut clen = 0usize;
            if bs_id[rare] == u32::MAX {
                clen = cnts[rare] as usize;
                let s = off[rare] as usize;
                cand[..clen].copy_from_slice(&u16_p[s..s + clen]);
            } else {
                let src = &bs[(bs_id[rare] as usize) * c_words..][..c_words];
                for w in 0..c_words {
                    let mut v = src[w];
                    while v != 0 { let id = (w << 6) + v.trailing_zeros() as usize; if id < c_docs { cand[clen] = id as u16; clen += 1; } v &= v - 1; }
                }
            }
            for &ot in &dist[1..nd] {
                if clen == 0 { break; }
                let (mut w, ns, ne) = (0usize, off[ot] as usize, off[ot] as usize + cnts[ot] as usize);
                if bs_id[ot] != u32::MAX {
                    let b = &bs[(bs_id[ot] as usize) * c_words..];
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
                let (mut sp, mut m) = (doc_off[ld] as usize, false);
                loop {
                    while *CMAP.get_unchecked(*d_buf.get_unchecked(sp) as usize) == 0 { sp += 1; }
                    if *d_buf.get_unchecked(sp) == b'\n' { break; }
                    let mut tail = [0u8; 32];
                    let (w0, w1, l) = read_tok(d_buf, &mut sp, &mut tail);
                    if l == pt[0].len as usize && w0 == pt[0].w0 && w1 == pt[0].w1 && (l <= 16 || &q_buf[pt[0].off as usize + 16..pt[0].off as usize + l] == &tail[..l - 16]) {
                        let (mut xp, mut j) = (sp, 1usize);
                        while j < k {
                            while *CMAP.get_unchecked(*d_buf.get_unchecked(xp) as usize) == 0 { xp += 1; }
                            if *d_buf.get_unchecked(xp) == b'\n' { break; }
                            let mut jtail = [0u8; 32];
                            let (jw0, jw1, jl) = read_tok(d_buf, &mut xp, &mut jtail);
                            if jl != pt[j].len as usize || jw0 != pt[j].w0 || jw1 != pt[j].w1 || (jl > 16 && &q_buf[pt[j].off as usize + 16..pt[j].off as usize + jl] != &jtail[..jl - 16]) { break; }
                            j += 1;
                        }
                        if j == k { m = true; break; }
                    }
                }
                if m { let d = d0 + ld as u32; r.cnt += 1; r.sum += d as u64; if r.t_cnt < 10 { r.top[r.t_cnt as usize] = d; r.t_cnt += 1; } }
            }
        }
    }
    let (pg_s, pg_e) = (c_byte[c] & !4095, c_byte[c + 1] & !4095);
    if pg_e > pg_s { madvise(d_buf.as_ptr().add(pg_s) as *mut u8, pg_e - pg_s, 4); }
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
    let mut tmp = [0u8; 20]; let mut i = 20;
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
        let mut i = 1;
        while i + 1 < args.len() {
            if args[i] == "--data" { d_dir = args[i + 1].clone(); } else if args[i] == "--out" { o_dir = args[i + 1].clone(); }
            i += 2;
        }
        let q_buf = map_file(&format!("{}/queries.txt", d_dir));
        let d_buf = map_file(&format!("{}/docs.txt", d_dir));

        let mut queries = Vec::with_capacity(16384);
        let (mut qp, qe) = (0usize, q_buf.len());
        while qp < qe {
            while qp < qe && (q_buf[qp] == b' ' || q_buf[qp] == b'\r' || q_buf[qp] == b'\n') { qp += 1; }
            if qp >= qe { break; }
            let ib = q_buf[qp..].starts_with(b"BOOL ");
            qp += if ib { 5 } else { 8 };
            let sep = if ib { b'\n' } else { b'"' };
            let mut le = qp;
            while le < qe && q_buf[le] != sep { le += 1; }
            let mut q = Query { kind: (!ib) as u8, k: 0, ops: 0, tids: [0; 8] };
            while qp < le {
                while qp < le && q_buf[qp] == b' ' { qp += 1; }
                if qp >= le { break; }
                let tk = qp;
                while qp < le && q_buf[qp] != b' ' { qp += 1; }
                let tok = &q_buf[tk..qp];
                if ib && tok == b"AND" { q.ops |= 1 << ((q.k - 1) * 2); }
                else if ib && tok == b"OR" { q.ops |= 2 << ((q.k - 1) * 2); }
                else if ib && tok == b"NOT" { q.ops |= 3 << ((q.k - 1) * 2); }
                else { q.tids[q.k as usize] = insert_term(tok, tk as u32, q_buf); q.k += 1; }
            }
            queries.push(q);
            qp = le;
            while qp < qe && q_buf[qp] != b'\n' { qp += 1; }
            qp += 1;
        }
        let mut rep_map = vec![u32::MAX; N_TERMS];
        for q in &queries {
            if q.kind == 1 {
                for idx in 0..(q.k as usize - 1) {
                    let t = q.tids[idx] as usize;
                    if t == q.tids[idx + 1] as usize && rep_map[t] == u32::MAX {
                        let rt = N_TERMS as u32; N_TERMS += 1; rep_map[t] = rt; TINFO[rt as usize] = TINFO[t];
                    }
                }
            }
        }

        let f_sz = d_buf.len();
        let mut pl = f_sz - 1;
        while pl > 0 && d_buf[pl - 1] != b'\n' { pl -= 1; }
        let mut total_docs = 0u32;
        while pl < f_sz && d_buf[pl] >= b'0' && d_buf[pl] <= b'9' { total_docs = total_docs * 10 + (d_buf[pl] - b'0') as u32; pl += 1; }
        total_docs += 1;

        let dpc = (total_docs + NUM_C as u32 - 1) / NUM_C as u32;
        let (mut doc_s, mut doc_e, mut c_byte) = ([0u32; NUM_C], [0u32; NUM_C], [0usize; NUM_C + 1]);
        c_byte[NUM_C] = f_sz;
        for c in 0..NUM_C {
            doc_s[c] = total_docs.min(if c > 0 { (c as u32 * dpc + 63) & !63 } else { 0 });
            if c > 0 { doc_e[c - 1] = doc_s[c]; }
        }
        doc_e[NUM_C - 1] = total_docs;
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
        std::thread::scope(|s| {
            for (c, chunk_out) in all_res.chunks_mut(n_q).enumerate() {
                let (q_ref, r_ref, ds, de, cb) = (&queries, &rep_map, &doc_s, &doc_e, &c_byte);
                s.spawn(move || process_chunk(c, d_buf, q_buf, q_ref, r_ref, ds, de, cb, chunk_out));
            }
        });

        munmap(d_buf.as_ptr() as *mut u8, f_sz);
        munmap(q_buf.as_ptr() as *mut u8, q_buf.len());

        let mut out_file = File::create(format!("{}/results.txt", o_dir)).unwrap();
        let mut wbuf = Vec::with_capacity(1 << 20);
        for qi in 0..n_q {
            let (mut cnt, mut sum, mut top, mut tc) = (0u64, 0u64, [0u32; 10], 0usize);
            for c in 0..NUM_C {
                let r = &all_res[c * n_q + qi];
                cnt += r.cnt as u64; sum += r.sum;
                for i in 0..(r.t_cnt as usize) { if tc < 10 { top[tc] = r.top[i]; tc += 1; } }
            }
            if cnt == 0 { wbuf.extend_from_slice(b"0 0\n"); }
            else {
                push_u64(&mut wbuf, cnt); wbuf.push(b' '); push_u64(&mut wbuf, sum);
                for i in 0..tc { wbuf.push(b' '); push_u64(&mut wbuf, top[i] as u64); }
                wbuf.push(b'\n');
            }
        }
        out_file.write_all(&wbuf).unwrap();
    }
}
