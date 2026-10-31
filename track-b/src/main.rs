use std::fs::File;
use std::io::Write;
use std::mem::MaybeUninit;
use std::os::unix::io::IntoRawFd;
use std::ptr;

extern "C" {
    fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
    fn munmap(a: *mut u8, l: usize) -> i32;
    fn close(fd: i32) -> i32;
    fn mallopt(param: i32, val: i32) -> i32;
}

const H_SZ: usize = 131072;
const H_MSK: usize = H_SZ - 1;
const NUM_C: usize = 12;
const BS_TH: u32 = 15625;
const MAX_W: usize = 384;

#[derive(Clone, Copy, Default)]
struct Slot { h32: u32, tid: u32 }
#[derive(Clone, Copy, Default)]
struct TInfo { w0: u64, w1: u64, off: u32, len: u16 }
#[derive(Clone, Copy)]
struct Query { kind: u8, k: u8, ops: u16, tids: [u32; 8] }

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
            if ti.w0 == w0 && ti.w1 == w1 && ti.len as usize == len && (len <= 16 || &q_buf[ti.off as usize + 16..ti.off as usize + len] == &tail[..len - 16]) {
                return s.tid;
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
unsafe fn read_tok(d: &[u8], p: &mut usize, tail: &mut [u8; 16]) -> (u64, u64, usize) {
    let (mut w0, mut w1, mut len) = (0u64, 0u64, 0usize);
    let mut m = *CMAP.get_unchecked(*d.get_unchecked(*p) as usize);
    loop {
        if len < 8 { w0 |= (m as u64) << (len * 8); }
        else if len < 16 { w1 |= (m as u64) << ((len - 8) * 8); }
        else if len < 32 { *tail.get_unchecked_mut(len - 16) = m; }
        len += 1; *p += 1;
        m = *CMAP.get_unchecked(*d.get_unchecked(*p) as usize);
        if m <= 1 { break; }
    }
    (w0, w1, len)
}

#[inline(always)]
unsafe fn scan_doc<F: FnMut(u32, bool)>(d_buf: &[u8], p: &mut usize, q_buf: &[u8], mut fn_: F) {
    let (mut pos, mut p_tid, mut p_pos) = (0u32, u32::MAX, u32::MAX);
    loop {
        while *CMAP.get_unchecked(*d_buf.get_unchecked(*p) as usize) == 0 { *p += 1; }
        if *d_buf.get_unchecked(*p) == b'\n' { *p += 1; break; }
        let mut tail = [0u8; 16];
        let (w0, w1, len) = read_tok(d_buf, p, &mut tail);
        let h = mix64(w0, w1, len);
        let tid = if (*BLOOM.get_unchecked(((h >> 32) as usize) & 8191) >> (h & 63)) & 1 != 0 {
            lookup_swar(w0, w1, &tail, len, q_buf, h)
        } else {
            u32::MAX
        };
        if tid != u32::MAX {
            fn_(tid, p_tid == tid && pos == p_pos + 1);
            p_tid = tid; p_pos = pos;
        } else {
            p_tid = u32::MAX;
        }
        pos += 1;
    }
}

const DIGITS: &[u8; 200] = b"00010203040506070809101112131415161718192021222324252627282930313233343536373839404142434445464748495051525354555657585960616263646566676869707172737475767778798081828384858687888990919293949596979899";

#[inline(always)]
fn push_u64(buf: &mut Vec<u8>, mut n: u64) {
    if n == 0 { buf.push(b'0'); return; }
    let mut tmp = [0u8; 20]; let mut i = 20;
    while n >= 100 {
        let rem = (n % 100) as usize * 2;
        n /= 100;
        i -= 2;
        tmp[i] = DIGITS[rem];
        tmp[i + 1] = DIGITS[rem + 1];
    }
    if n < 10 {
        i -= 1; tmp[i] = b'0' + n as u8;
    } else {
        let rem = n as usize * 2;
        i -= 2;
        tmp[i] = DIGITS[rem];
        tmp[i + 1] = DIGITS[rem + 1];
    }
    buf.extend_from_slice(&tmp[i..]);
}

#[inline(always)]
fn fmt_res(cnt: u64, sum: u64, top: &[u32; 10], tc: usize, out: &mut Vec<u8>) {
    if cnt == 0 {
        out.extend_from_slice(b"0 0\n");
    } else {
        push_u64(out, cnt); out.push(b' ');
        push_u64(out, sum);
        for i in 0..tc { out.push(b' '); push_u64(out, top[i] as u64); }
        out.push(b'\n');
    }
}

unsafe fn map_file(path: &str) -> &'static [u8] {
    let f = File::open(path).unwrap();
    let len = f.metadata().unwrap().len() as usize;
    let fd = f.into_raw_fd();
    let p = mmap(ptr::null_mut(), len, 1, 1, fd, 0);
    close(fd);
    std::slice::from_raw_parts(p, len)
}

#[target_feature(enable = "avx2,bmi1,bmi2,lzcnt,popcnt")]
unsafe fn exec_bool(q: &Query, c_off: &[u32], c_base: &[u32; NUM_C], is_bs: &[bool], bs_idx: &[u32], doc_s: &[u32; NUM_C], doc_e: &[u32; NUM_C], w_per_doc: usize, g_bs: &[u64], g_u16: &[u16], nt: usize, out: &mut Vec<u8>) {
    let (mut t_cnt, mut t_sum, mut top, mut tc) = (0u64, 0u64, [0u32; 10], 0usize);
    let mut clist = [0u16; 1024];
    let mut curr_uninit = MaybeUninit::<[u64; MAX_W]>::uninit();
    let mut nb_uninit = MaybeUninit::<[u64; MAX_W]>::uninit();
    let curr = curr_uninit.assume_init_mut();
    let nb = nb_uninit.assume_init_mut();

    for c in 0..NUM_C {
        let (cs, cd) = (doc_s[c], doc_e[c] - doc_s[c]);
        if cd == 0 { continue; }
        let cw = ((cd + 63) >> 6) as usize;
        let c_base_off = c * nt;
        let t0 = q.tids[0] as usize;
        let mut clen = 0usize;
        let mut in_bs = is_bs[t0];

        let get_c_bs = |tid: usize, buf: &mut [u64; MAX_W]| {
            if tid == u32::MAX as usize { buf[..cw].fill(0); return; }
            if is_bs[tid] {
                let off = (bs_idx[tid] as usize) * w_per_doc + ((cs >> 6) as usize);
                buf[..cw].copy_from_slice(&g_bs[off..off + cw]);
            } else {
                buf[..cw].fill(0);
                let s = if tid > 0 { *c_off.get_unchecked(c_base_off + tid - 1) } else { c_base[c] } as usize;
                let e = *c_off.get_unchecked(c_base_off + tid) as usize;
                for &ld in &g_u16[s..e] { *buf.get_unchecked_mut((ld as usize) >> 6) |= 1u64 << (ld & 63); }
            }
        };

        if in_bs {
            get_c_bs(t0, curr);
        } else {
            let s0 = if t0 > 0 { *c_off.get_unchecked(c_base_off + t0 - 1) } else { c_base[c] } as usize;
            clen = (*c_off.get_unchecked(c_base_off + t0) as usize) - s0;
            if clen > 1024 { in_bs = true; get_c_bs(t0, curr); }
            else { clist[..clen].copy_from_slice(&g_u16[s0..s0 + clen]); }
        }

        for oi in 0..(q.k as usize - 1) {
            let op = (q.ops >> (oi * 2)) & 3;
            let nt_id = q.tids[oi + 1] as usize;
            let ns = if nt_id > 0 { *c_off.get_unchecked(c_base_off + nt_id - 1) } else { c_base[c] } as usize;
            let ne = *c_off.get_unchecked(c_base_off + nt_id) as usize;
            if in_bs {
                if is_bs[nt_id] {
                    let b = &g_bs[(bs_idx[nt_id] as usize) * w_per_doc + ((cs >> 6) as usize)..];
                    if op == 1 { for w in 0..cw { *curr.get_unchecked_mut(w) &= *b.get_unchecked(w); } }
                    else if op == 2 { for w in 0..cw { *curr.get_unchecked_mut(w) |= *b.get_unchecked(w); } }
                    else { for w in 0..cw { *curr.get_unchecked_mut(w) &= !*b.get_unchecked(w); } }
                } else if op == 2 {
                    for &ld in &g_u16[ns..ne] { *curr.get_unchecked_mut((ld as usize) >> 6) |= 1u64 << (ld & 63); }
                } else if op == 3 {
                    for &ld in &g_u16[ns..ne] { *curr.get_unchecked_mut((ld as usize) >> 6) &= !(1u64 << (ld & 63)); }
                } else {
                    get_c_bs(nt_id, nb);
                    for w in 0..cw { *curr.get_unchecked_mut(w) &= *nb.get_unchecked(w); }
                }
            } else if op == 1 {
                if clen == 0 { continue; }
                let mut w = 0usize;
                if is_bs[nt_id] {
                    let b = &g_bs[(bs_idx[nt_id] as usize) * w_per_doc..];
                    for i in 0..clen {
                        let d = (cs + clist[i] as u32) as usize;
                        if ((*b.get_unchecked(d >> 6) >> (d & 63)) & 1) != 0 { *clist.get_unchecked_mut(w) = clist[i]; w += 1; }
                    }
                } else {
                    let (mut i, mut j) = (0usize, ns);
                    while i < clen && j < ne {
                        if clist[i] == g_u16[j] { *clist.get_unchecked_mut(w) = clist[i]; w += 1; i += 1; j += 1; }
                        else if clist[i] < g_u16[j] { i += 1; } else { j += 1; }
                    }
                }
                clen = w;
            } else if op == 3 {
                if clen == 0 { continue; }
                let mut w = 0usize;
                if is_bs[nt_id] {
                    let b = &g_bs[(bs_idx[nt_id] as usize) * w_per_doc..];
                    for i in 0..clen {
                        let d = (cs + clist[i] as u32) as usize;
                        if ((*b.get_unchecked(d >> 6) >> (d & 63)) & 1) == 0 { *clist.get_unchecked_mut(w) = clist[i]; w += 1; }
                    }
                } else {
                    let (mut i, mut j) = (0usize, ns);
                    while i < clen && j < ne {
                        if clist[i] == g_u16[j] { i += 1; j += 1; }
                        else if clist[i] < g_u16[j] { *clist.get_unchecked_mut(w) = clist[i]; w += 1; i += 1; } else { j += 1; }
                    }
                    while i < clen { *clist.get_unchecked_mut(w) = clist[i]; w += 1; i += 1; }
                }
                clen = w;
            } else {
                let nlen = if is_bs[nt_id] { 100000 } else { ne - ns };
                if is_bs[nt_id] || clen + nlen > 1024 {
                    get_c_bs(nt_id, curr);
                    for i in 0..clen { *curr.get_unchecked_mut((clist[i] as usize) >> 6) |= 1u64 << (clist[i] & 63); }
                    in_bs = true;
                } else {
                    let mut tmp = [0u16; 1024];
                    let (mut i, mut j, mut w) = (0usize, ns, 0usize);
                    while i < clen && j < ne {
                        if clist[i] == g_u16[j] { tmp[w] = clist[i]; w += 1; i += 1; j += 1; }
                        else if clist[i] < g_u16[j] { tmp[w] = clist[i]; w += 1; i += 1; } else { tmp[w] = g_u16[j]; w += 1; j += 1; }
                    }
                    while i < clen { tmp[w] = clist[i]; w += 1; i += 1; }
                    while j < ne { tmp[w] = g_u16[j]; w += 1; j += 1; }
                    clist[..w].copy_from_slice(&tmp[..w]); clen = w;
                }
            }
        }

        if !in_bs {
            t_cnt += clen as u64;
            for i in 0..clen {
                let d = cs + clist[i] as u32; t_sum += d as u64;
                if tc < 10 { top[tc] = d; tc += 1; }
            }
        } else {
            if (cd & 63) != 0 { *curr.get_unchecked_mut(cw - 1) &= (1u64 << (cd & 63)) - 1; }
            for w in 0..cw {
                let mut v = *curr.get_unchecked(w); if v == 0 { continue; }
                t_cnt += v.count_ones() as u64;
                let base = cs + ((w as u32) << 6);
                while v != 0 {
                    let d = base + v.trailing_zeros(); t_sum += d as u64;
                    if tc < 10 { top[tc] = d; tc += 1; }
                    v &= v - 1;
                }
            }
        }
    }
    fmt_res(t_cnt, t_sum, &top, tc, out);
}

#[target_feature(enable = "avx2,bmi1,bmi2,lzcnt,popcnt")]
unsafe fn exec_phrase(q: &Query, c_off: &[u32], c_base: &[u32; NUM_C], is_bs: &[bool], bs_idx: &[u32], doc_s: &[u32; NUM_C], doc_e: &[u32; NUM_C], w_per_doc: usize, g_bs: &[u64], g_u16: &[u16], term_df: &[u32], rep_map: &[u32], doc_ts: &[u32], d_buf: &[u8], q_buf: &[u8], nt: usize, out: &mut Vec<u8>) {
    let (k, mut nd, mut dist) = (q.k as usize, 0usize, [0usize; 16]);
    for i in 0..k {
        let t = q.tids[i] as usize;
        if t == u32::MAX as usize || *term_df.get_unchecked(t) == 0 { fmt_res(0, 0, &[0; 10], 0, out); return; }
        if !dist[..nd].contains(&t) { dist[nd] = t; nd += 1; }
        if i + 1 < k && t == q.tids[i + 1] as usize && t < rep_map.len() && *rep_map.get_unchecked(t) != u32::MAX {
            let rt = *rep_map.get_unchecked(t) as usize;
            if *term_df.get_unchecked(rt) == 0 { fmt_res(0, 0, &[0; 10], 0, out); return; }
            if !dist[..nd].contains(&rt) { dist[nd] = rt; nd += 1; }
        }
    }
    dist[..nd].sort_unstable_by_key(|&t| *term_df.get_unchecked(t));
    let mut pt = [TInfo::default(); 8];
    for i in 0..k { pt[i] = *TINFO.get_unchecked(q.tids[i] as usize); }

    let (mut t_cnt, mut t_sum, mut top, mut tc) = (0u64, 0u64, [0u32; 10], 0usize);
    let mut cand_uninit = MaybeUninit::<[u16; 21000]>::uninit();
    let cand = cand_uninit.assume_init_mut();

    for c in 0..NUM_C {
        let (cs, cd) = (doc_s[c], doc_e[c] - doc_s[c]);
        if cd == 0 { continue; }
        let c_base_off = c * nt;
        let (rare, mut clen) = (dist[0], 0usize);
        if !is_bs[rare] {
            let s = if rare > 0 { *c_off.get_unchecked(c_base_off + rare - 1) } else { c_base[c] } as usize;
            clen = (*c_off.get_unchecked(c_base_off + rare) as usize) - s;
            if clen == 0 { continue; }
            cand[..clen].copy_from_slice(&g_u16[s..s + clen]);
        } else {
            let cw = ((cd + 63) >> 6) as usize;
            let src = &g_bs[(bs_idx[rare] as usize) * w_per_doc + ((cs >> 6) as usize)..];
            for w in 0..cw {
                let mut v = *src.get_unchecked(w);
                while v != 0 {
                    let id = (w << 6) + v.trailing_zeros() as usize;
                    if id < cd as usize { *cand.get_unchecked_mut(clen) = id as u16; clen += 1; }
                    v &= v - 1;
                }
            }
            if clen == 0 { continue; }
        }

        for &ot in &dist[1..nd] {
            if clen == 0 { break; }
            let mut w = 0usize;
            if is_bs[ot] {
                let b = &g_bs[(bs_idx[ot] as usize) * w_per_doc..];
                for i in 0..clen {
                    let d = (cs + cand[i] as u32) as usize;
                    if ((*b.get_unchecked(d >> 6) >> (d & 63)) & 1) != 0 { *cand.get_unchecked_mut(w) = cand[i]; w += 1; }
                }
            } else {
                let ns = if ot > 0 { *c_off.get_unchecked(c_base_off + ot - 1) } else { c_base[c] } as usize;
                let ne = *c_off.get_unchecked(c_base_off + ot) as usize;
                let (mut i, mut j) = (0usize, ns);
                while i < clen && j < ne {
                    if cand[i] == g_u16[j] { *cand.get_unchecked_mut(w) = cand[i]; w += 1; i += 1; j += 1; }
                    else if cand[i] < g_u16[j] { i += 1; } else { j += 1; }
                }
            }
            clen = w;
        }

        for ci in 0..clen {
            let d = cs + cand[ci] as u32;
            let mut sp = *doc_ts.get_unchecked(d as usize) as usize;
            let mut matched = false;
            loop {
                while *CMAP.get_unchecked(*d_buf.get_unchecked(sp) as usize) == 0 { sp += 1; }
                if *d_buf.get_unchecked(sp) == b'\n' { break; }
                let mut tail = [0u8; 16];
                let (w0, w1, l) = read_tok(d_buf, &mut sp, &mut tail);
                if l == pt[0].len as usize && w0 == pt[0].w0 && w1 == pt[0].w1 && (l <= 16 || &q_buf[pt[0].off as usize + 16..pt[0].off as usize + l] == &tail[..l - 16]) {
                    let (mut xp, mut j) = (sp, 1usize);
                    while j < k {
                        while *CMAP.get_unchecked(*d_buf.get_unchecked(xp) as usize) == 0 { xp += 1; }
                        if *d_buf.get_unchecked(xp) == b'\n' { break; }
                        let mut jtail = [0u8; 16];
                        let (jw0, jw1, jl) = read_tok(d_buf, &mut xp, &mut jtail);
                        if jl != pt[j].len as usize || jw0 != pt[j].w0 || jw1 != pt[j].w1 || (jl > 16 && &q_buf[pt[j].off as usize + 16..pt[j].off as usize + jl] != &jtail[..jl - 16]) { break; }
                        j += 1;
                    }
                    if j == k { matched = true; break; }
                }
            }
            if matched {
                t_cnt += 1; t_sum += d as u64;
                if tc < 10 { top[tc] = d; tc += 1; }
            }
        }
    }
    fmt_res(t_cnt, t_sum, &top, tc, out);
}

fn main() {
    unsafe {
        mallopt(-8, 2); // M_ARENA_MAX = 2
        mallopt(-4, 0); // M_TRIM_THRESHOLD = 0
        CMAP[b'\n' as usize] = 1;
        for c in b'0'..=b'9' { CMAP[c as usize] = c; }
        for c in b'a'..=b'z' { CMAP[c as usize] = c; CMAP[(c - 32) as usize] = c; }

        let (mut d_dir, mut o_dir) = ("/data".to_string(), "/out".to_string());
        let args: Vec<String> = std::env::args().collect();
        let mut i = 1;
        while i + 1 < args.len() {
            if args[i] == "--data" { d_dir = args[i + 1].clone(); }
            else if args[i] == "--out" { o_dir = args[i + 1].clone(); }
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
                        let rt = N_TERMS as u32; N_TERMS += 1;
                        rep_map[t] = rt; TINFO[rt as usize] = TINFO[t];
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

        let mut doc_ts = vec![0u32; total_docs as usize];
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

        let nt = N_TERMS;
        let mut sc = vec![0u32; NUM_C * nt];
        let doc_ts_ptr = doc_ts.as_mut_ptr() as usize;

        std::thread::scope(|s| {
            for (c, sc_c) in sc.chunks_mut(nt).enumerate() {
                let (r_ref, ds, de, cb) = (&rep_map, &doc_s, &doc_e, &c_byte);
                s.spawn(move || {
                    if ds[c] >= de[c] { return; }
                    let mut seen = vec![u32::MAX; nt];
                    let r_sz = r_ref.len();
                    let (mut p, pe) = (cb[c], cb[c + 1]);
                    let d_ts = doc_ts_ptr as *mut u32;
                    while p < pe {
                        let mut d = 0u32;
                        while *d_buf.get_unchecked(p) >= b'0' && *d_buf.get_unchecked(p) <= b'9' { d = d * 10 + (*d_buf.get_unchecked(p) - b'0') as u32; p += 1; }
                        if *d_buf.get_unchecked(p) == b'\t' { p += 1; }
                        *d_ts.add(d as usize) = p as u32;
                        scan_doc(d_buf, &mut p, q_buf, |tid, rep| {
                            let t = tid as usize;
                            if *seen.get_unchecked(t) != d { *seen.get_unchecked_mut(t) = d; *sc_c.get_unchecked_mut(t) += 1; }
                            if rep && t < r_sz && *r_ref.get_unchecked(t) != u32::MAX {
                                let rt = *r_ref.get_unchecked(t) as usize;
                                if *seen.get_unchecked(rt) != d { *seen.get_unchecked_mut(rt) = d; *sc_c.get_unchecked_mut(rt) += 1; }
                            }
                        });
                    }
                });
            }
        });

        let mut term_df = vec![0u32; nt];
        for c in 0..NUM_C {
            for i in 0..nt { term_df[i] += sc[c * nt + i]; }
        }

        let (mut is_bs, mut bs_idx, mut n_bs) = (vec![false; nt], vec![u32::MAX; nt], 0usize);
        for i in 0..nt {
            if term_df[i] > BS_TH { is_bs[i] = true; bs_idx[i] = n_bs as u32; n_bs += 1; }
        }

        let w_per_doc = ((total_docs + 63) >> 6) as usize;
        let mut tot_u16 = 0u32;
        let mut c_base = [0u32; NUM_C];
        let mut c_off = sc;

        for c in 0..NUM_C {
            c_base[c] = tot_u16;
            let c_off_slice = &mut c_off[c * nt..(c + 1) * nt];
            for i in 0..nt {
                if !is_bs[i] { tot_u16 += c_off_slice[i]; }
                c_off_slice[i] = tot_u16;
            }
        }

        let mut g_bs = vec![0u64; n_bs * w_per_doc];
        let mut g_u16 = vec![0u16; tot_u16 as usize];

        let g_bs_ptr = g_bs.as_mut_ptr() as usize;
        let g_u16_ptr = g_u16.as_mut_ptr() as usize;

        std::thread::scope(|s| {
            for c in 0..NUM_C {
                let (ib, bi, co_ref, ds, de, cb, r_ref) = (&is_bs, &bs_idx, &c_off, &doc_s, &doc_e, &c_byte, &rep_map);
                s.spawn(move || {
                    let d0 = ds[c];
                    if d0 >= de[c] { return; }
                    let bs_ptr = g_bs_ptr as *mut u64;
                    let u16_ptr = g_u16_ptr as *mut u16;
                    let c_base_off = c * nt;
                    let r_sz = r_ref.len();
                    let (mut p, pe) = (cb[c], cb[c + 1]);

                    // Initialize local write pointers for chunk c
                    let mut cur_pos = vec![0u32; nt];
                    for i in 0..nt {
                        cur_pos[i] = if i > 0 { *co_ref.get_unchecked(c_base_off + i - 1) } else { c_base[c] };
                    }

                    let mut emit = |tid: u32, d: u32, ld: u16| {
                        let t = tid as usize;
                        if *ib.get_unchecked(t) {
                            let off = (*bi.get_unchecked(t) as usize) * w_per_doc + ((d >> 6) as usize);
                            *bs_ptr.add(off) |= 1u64 << (d & 63);
                        } else {
                            let cur = *cur_pos.get_unchecked(t);
                            let start = if t > 0 { *co_ref.get_unchecked(c_base_off + t - 1) } else { c_base[c] };
                            if cur == start || *u16_ptr.add((cur - 1) as usize) != ld {
                                *u16_ptr.add(cur as usize) = ld;
                                *cur_pos.get_unchecked_mut(t) = cur + 1;
                            }
                        }
                    };

                    while p < pe {
                        let mut d = 0u32;
                        while *d_buf.get_unchecked(p) >= b'0' && *d_buf.get_unchecked(p) <= b'9' { d = d * 10 + (*d_buf.get_unchecked(p) - b'0') as u32; p += 1; }
                        if *d_buf.get_unchecked(p) == b'\t' { p += 1; }
                        let ld = (d - d0) as u16;
                        scan_doc(d_buf, &mut p, q_buf, |tid, rep| {
                            emit(tid, d, ld);
                            if rep && (tid as usize) < r_sz && *r_ref.get_unchecked(tid as usize) != u32::MAX {
                                emit(*r_ref.get_unchecked(tid as usize), d, ld);
                            }
                        });
                    }
                });
            }
        });

        let n_q = queries.len();
        let chunk_q = (n_q + NUM_C - 1) / NUM_C;
        let mut out_bufs = vec![Vec::<u8>::with_capacity(chunk_q * 48); NUM_C];

        std::thread::scope(|s| {
            for (t, ob) in out_bufs.iter_mut().enumerate() {
                let qs = t * chunk_q;
                let qe = (qs + chunk_q).min(n_q);
                if qs >= qe { continue; }
                let (q_slice, co, cb, ib, bi, ds, de, bs, u16s, tdf, rm, dts) = (&queries[qs..qe], &c_off, &c_base, &is_bs, &bs_idx, &doc_s, &doc_e, &g_bs, &g_u16, &term_df, &rep_map, &doc_ts);
                s.spawn(move || {
                    for q in q_slice {
                        if q.kind == 0 {
                            exec_bool(q, co, cb, ib, bi, ds, de, w_per_doc, bs, u16s, nt, ob);
                        } else {
                            exec_phrase(q, co, cb, ib, bi, ds, de, w_per_doc, bs, u16s, tdf, rm, dts, d_buf, q_buf, nt, ob);
                        }
                    }
                });
            }
        });

        drop(g_bs);
        drop(g_u16);
        drop(c_off);
        drop(queries);
        drop(doc_ts);
        drop(term_df);
        drop(rep_map);

        munmap(d_buf.as_ptr() as *mut u8, f_sz);
        munmap(q_buf.as_ptr() as *mut u8, q_buf.len());

        let mut out_file = File::create(format!("{}/results.txt", o_dir)).unwrap();
        for ob in &out_bufs {
            out_file.write_all(ob).unwrap();
        }
    }
}
