#![allow(static_mut_refs)]
use std::fs::File;
use std::io::Write;
use std::os::unix::io::IntoRawFd;
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};

unsafe extern "C" {
    fn pread(fd: i32, buf: *mut u8, count: usize, offset: i64) -> isize;
    fn close(fd: i32) -> i32;
}

const H_SZ: usize = 131072;
const H_MSK: usize = H_SZ - 1;
const NUM_C: usize = 24;
const BS_TH: u32 = 512;
const MAX_W: usize = 256;

#[derive(Clone, Copy, Default)]
struct Slot { h32: u32, tid: u32 }
#[derive(Clone, Copy, Default)]
struct TInfo { w0: u64, w1: u64, off: u32, len: u16, in_phr: u8 }
#[derive(Clone, Copy)]
struct Query { kind: u8, k: u8, ops: u16, tids: [u32; 8] }
#[derive(Clone, Copy, Default)]
struct CRes { cnt: u32, sum: u64, top: [u32; 10], tc: u8 }

static mut CMAP: [u8; 256] = [0; 256];
static mut VTAB: [Slot; H_SZ] = [Slot { h32: 0, tid: 0 }; H_SZ];
static mut TINFO: [TInfo; 81920] = [TInfo { w0: 0, w1: 0, off: 0, len: 0, in_phr: 0 }; 81920];
static mut BLOOM: [u64; 8192] = [0; 8192];
static mut N_TERMS: usize = 0;

struct Arena {
    cnts: Vec<u32>, seen: Vec<u32>, off: Vec<u32>, cur_off: Vec<u32>, bs_id: Vec<u32>,
    doc_p_off: Vec<u32>, raw_post: Vec<u32>, phr_toks: Vec<u64>, bs: Vec<u64>, u16_p: Vec<u16>, buf: Vec<u8>,
}

impl Arena {
    fn new(nt: usize, max_docs: usize) -> Self {
        Self {
            cnts: vec![0; nt], seen: vec![u32::MAX; nt], off: vec![0; nt], cur_off: vec![0; nt], bs_id: vec![u32::MAX; nt],
            doc_p_off: vec![0; max_docs + 2], raw_post: Vec::with_capacity(max_docs * 20),
            phr_toks: Vec::with_capacity(max_docs * 6), bs: Vec::new(), u16_p: Vec::new(), buf: vec![0u8; (1 << 18) + 64],
        }
    }
}

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
            if ti.w0 == w0 && ti.w1 == w1 && ti.len as usize == len
                && (len <= 16 || &q_buf[ti.off as usize + 16..ti.off as usize + len] == &tail[..len - 16])
            {
                return s.tid;
            }
        }
        idx = (idx + 1) & H_MSK;
    }
    u32::MAX
}

#[target_feature(enable = "avx2,bmi1,bmi2,lzcnt,popcnt")]
unsafe fn process_chunk(c: usize, fd_d: i32, q_buf: &[u8], queries: &[Query], rep_map: &[u32], doc_s: &[u32; NUM_C], doc_e: &[u32; NUM_C], c_byte: &[usize; NUM_C + 1], ar: &mut Arena, my_res: &mut [CRes]) {
    let (d0, c_docs) = (doc_s[c], (doc_e[c] - doc_s[c]) as usize);
    if c_docs == 0 { return; }
    let (nt, cw, r_sz) = (N_TERMS, (c_docs + 63) >> 6, rep_map.len());
    ar.cnts.fill(0); ar.seen.fill(u32::MAX); ar.raw_post.clear(); ar.phr_toks.clear();

    const BUF_SZ: usize = 1 << 18;
    let (mut f_pos, f_end, mut b_len, mut b_pos) = (c_byte[c], c_byte[c + 1], 0usize, 0usize);
    let refill = |buf: &mut [u8], f_pos: &mut usize, b_len: &mut usize, b_pos: &mut usize| {
        let rem = *b_len - *b_pos;
        if rem > 0 && *b_pos > 0 { buf.copy_within(*b_pos..*b_len, 0); }
        *b_len = rem; *b_pos = 0;
        if *f_pos < f_end {
            let to_read = (BUF_SZ - *b_len).min(f_end - *f_pos);
            let r = pread(fd_d, buf.as_mut_ptr().add(*b_len), to_read, *f_pos as i64);
            if r > 0 { *b_len += r as usize; *f_pos += r as usize; }
        }
        buf[*b_len] = b'\n';
    };
    refill(&mut ar.buf, &mut f_pos, &mut b_len, &mut b_pos);

    while b_pos < b_len || f_pos < f_end {
        if b_len - b_pos < 65536 && f_pos < f_end { refill(&mut ar.buf, &mut f_pos, &mut b_len, &mut b_pos); }
        if b_pos >= b_len { break; }
        let (mut p, mut d) = (b_pos, 0u32);
        while *ar.buf.get_unchecked(p) >= b'0' && *ar.buf.get_unchecked(p) <= b'9' {
            d = d * 10 + (*ar.buf.get_unchecked(p) - b'0') as u32; p += 1;
        }
        if *ar.buf.get_unchecked(p) == b'\t' { p += 1; }
        let ld = (d - d0) as usize;
        *ar.doc_p_off.get_unchecked_mut(ld) = ar.phr_toks.len() as u32;
        let (mut pos, mut p_tid, mut p_pos) = (0u32, u32::MAX, u32::MAX);

        loop {
            while *CMAP.get_unchecked(*ar.buf.get_unchecked(p) as usize) == 0 { p += 1; }
            if *ar.buf.get_unchecked(p) == b'\n' { p += 1; break; }
            let (mut w0, mut w1, mut len, mut tail) = (0u64, 0u64, 0usize, [0u8; 32]);
            let mut m = *CMAP.get_unchecked(*ar.buf.get_unchecked(p) as usize);
            loop {
                if len < 8 { w0 |= (m as u64) << (len * 8); }
                else if len < 16 { w1 |= (m as u64) << ((len - 8) * 8); }
                else if len < 48 { *tail.get_unchecked_mut(len - 16) = m; }
                len += 1; p += 1;
                m = *CMAP.get_unchecked(*ar.buf.get_unchecked(p) as usize);
                if m <= 1 { break; }
            }
            let h = mix64(w0, w1, len);
            let tid = if (*BLOOM.get_unchecked(((h >> 32) as usize) & 8191) >> (h & 63)) & 1 != 0 {
                lookup_swar(w0, w1, &tail, len, q_buf, h)
            } else { u32::MAX };
            if tid != u32::MAX {
                let t = tid as usize;
                if *ar.seen.get_unchecked(t) != d {
                    *ar.seen.get_unchecked_mut(t) = d;
                    *ar.cnts.get_unchecked_mut(t) += 1;
                    ar.raw_post.push((tid << 14) | (ld as u32));
                }
                if TINFO.get_unchecked(t).in_phr != 0 { ar.phr_toks.push(((pos as u64) << 32) | (tid as u64)); }
                if p_tid == tid && pos == p_pos + 1 && t < r_sz {
                    let rt = *rep_map.get_unchecked(t);
                    if rt != u32::MAX && *ar.seen.get_unchecked(rt as usize) != d {
                        *ar.seen.get_unchecked_mut(rt as usize) = d;
                        *ar.cnts.get_unchecked_mut(rt as usize) += 1;
                        ar.raw_post.push((rt << 14) | (ld as u32));
                    }
                }
                p_tid = tid; p_pos = pos;
            } else { p_tid = u32::MAX; }
            pos += 1;
        }
        *ar.doc_p_off.get_unchecked_mut(ld + 1) = ar.phr_toks.len() as u32;
        b_pos = p;
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
        let (t, ld) = ((rp >> 14) as usize, (rp & 0x3FFF) as usize);
        let b = *ar.bs_id.get_unchecked(t);
        if b != u32::MAX { *ar.bs.get_unchecked_mut((b as usize) * cw + (ld >> 6)) |= 1u64 << (ld & 63); }
        else {
            let o = *ar.cur_off.get_unchecked(t);
            *ar.cur_off.get_unchecked_mut(t) = o + 1;
            *ar.u16_p.get_unchecked_mut(o as usize) = ld as u16;
        }
    }

    let (cnts, off, bs_id, bs, u16_p, doc_p_off, phr_toks) = (&ar.cnts, &ar.off, &ar.bs_id, &ar.bs, &ar.u16_p, &ar.doc_p_off, &ar.phr_toks);
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

    let (mut clist, mut tmp, mut cand) = ([0u16; (BS_TH as usize) + 1], [0u16; (BS_TH as usize) + 1], [0u16; 16384]);
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
            let t0 = q.tids[0];
            for ci in 0..clen {
                let ld = cand[ci] as usize;
                if k == 1 { let d = d0 + ld as u32; r.cnt += 1; r.sum += d as u64; if r.tc < 10 { r.top[r.tc as usize] = d; r.tc += 1; } continue; }
                let (ps, pe) = (doc_p_off[ld] as usize, doc_p_off[ld + 1] as usize);
                let mut m = false;
                if pe - ps >= k {
                    for pi in ps..=(pe - k) {
                        let pk = *phr_toks.get_unchecked(pi);
                        if (pk as u32) == t0 {
                            let (base_pos, mut j) = ((pk >> 32) as u32, 1usize);
                            while j < k {
                                if *phr_toks.get_unchecked(pi + j) != (((base_pos + j as u32) as u64) << 32) | (q.tids[j] as u64) { break; }
                                j += 1;
                            }
                            if j == k { m = true; break; }
                        }
                    }
                }
                if m { let d = d0 + ld as u32; r.cnt += 1; r.sum += d as u64; if r.tc < 10 { r.top[r.tc as usize] = d; r.tc += 1; } }
            }
        }
    }
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
        let q_vec = std::fs::read(format!("{}/queries.txt", d_dir)).unwrap();
        let q_buf = q_vec.as_slice();
        let f_d = File::open(format!("{}/docs.txt", d_dir)).unwrap();
        let f_sz = f_d.metadata().unwrap().len() as usize;
        let fd_d = f_d.into_raw_fd();

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
                    let mut tid = lookup_swar(w0, w1, if len > 16 { &tok[16..] } else { &[] }, len, q_buf, h);
                    if tid == u32::MAX {
                        let mut idx = (h as usize) & H_MSK;
                        while VTAB[idx].h32 != 0 { idx = (idx + 1) & H_MSK; }
                        tid = N_TERMS as u32; N_TERMS += 1;
                        VTAB[idx] = Slot { h32: ((h ^ (h >> 32)) as u32) | 1, tid };
                        TINFO[tid as usize] = TInfo { w0, w1, off, len: len as u16, in_phr: 0 };
                        BLOOM[((h >> 32) as usize) & 8191] |= 1u64 << (h & 63);
                    }
                    if !ib { TINFO[tid as usize].in_phr = 1; }
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

        let mut tail_buf = [0u8; 4096];
        let t_read = 4096.min(f_sz);
        pread(fd_d, tail_buf.as_mut_ptr(), t_read, (f_sz - t_read) as i64);
        let mut pl = t_read - 1;
        while pl > 0 && tail_buf[pl - 1] != b'\n' { pl -= 1; }
        let mut total_docs = 0u32;
        while pl < t_read && tail_buf[pl] >= b'0' && tail_buf[pl] <= b'9' {
            total_docs = total_docs * 10 + (tail_buf[pl] - b'0') as u3