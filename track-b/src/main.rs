#![allow(static_mut_refs)]
use std::{fs::File, io::Write, mem::MaybeUninit, os::unix::io::IntoRawFd, ptr};

extern "C" {
    fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
    fn close(fd: i32) -> i32;
    fn mallopt(p: i32, v: i32) -> i32;
}

const H_SZ: usize = 131072;
const H_MSK: usize = H_SZ - 1;
const BLOOM_SZ: usize = 16384;
const BLOOM_MSK: usize = BLOOM_SZ - 1;
const NUM_C: usize = 12;
const BS_TH: u32 = 15625;
const MAX_W: usize = 350;

#[derive(Clone, Copy, Default)]
struct Slot(u32, u32);
#[derive(Clone, Copy, Default)]
struct TInfo(u64, u64, u32, u16);
#[derive(Clone, Copy)]
struct Query(u8, u8, u16, [u32; 8]);

const CMAP: [u8; 256] = {
    let mut m = [0u8; 256];
    m[b'\n' as usize] = 1;
    let mut i = 0u8;
    loop {
        if i >= b'0' && i <= b'9' {
            m[i as usize] = i;
        }
        if i >= b'a' && i <= b'z' {
            m[i as usize] = i;
            m[(i - 32) as usize] = i;
        }
        if i == 255 {
            break;
        }
        i += 1;
    }
    m
};

static mut VTAB: [Slot; H_SZ] = [Slot(0, 0); H_SZ];
static mut TINFO: [TInfo; 81920] = [TInfo(0, 0, 0, 0); 81920];
static mut BLOOM: [u64; BLOOM_SZ] = [0; BLOOM_SZ];
static mut N_TERMS: usize = 0;

#[inline(always)]
fn mix64(w0: u64, w1: u64, len: usize) -> u64 {
    let h = (w0 ^ (w1.wrapping_mul(0x9e3779b97f4a7c15).wrapping_add(len as u64)))
        .wrapping_mul(0xbf58476d1ce4e5b9);
    h ^ (h >> 31)
}

#[inline(always)]
unsafe fn lookup_swar(w0: u64, w1: u64, tail: &[u8], len: usize, q_buf: &[u8], h: u64) -> u32 {
    let (h32, mut idx) = (((h ^ (h >> 32)) as u32) | 1, (h as usize) & H_MSK);
    loop {
        let s = *VTAB.get_unchecked(idx);
        if s.0 == 0 {
            return u32::MAX;
        }
        if s.0 == h32 {
            let ti = *TINFO.get_unchecked(s.1 as usize);
            if ti.0 == w0
                && ti.1 == w1
                && ti.3 as usize == len
                && (len <= 16
                    || &q_buf[ti.2 as usize + 16..ti.2 as usize + len] == &tail[..len - 16])
            {
                return s.1;
            }
        }
        idx = (idx + 1) & H_MSK;
    }
}

unsafe fn insert_term(s: &[u8], off: u32, q_buf: &[u8]) -> u32 {
    let (mut w0, mut w1, len) = (0u64, 0u64, s.len());
    ptr::copy_nonoverlapping(s.as_ptr(), &mut w0 as *mut _ as *mut u8, len.min(8));
    if len > 8 {
        ptr::copy_nonoverlapping(
            s.as_ptr().add(8),
            &mut w1 as *mut _ as *mut u8,
            (len - 8).min(8),
        );
    }
    let h = mix64(w0, w1, len);
    let tail = if len > 16 { &s[16..] } else { &[] };
    let tid = lookup_swar(w0, w1, tail, len, q_buf, h);
    if tid != u32::MAX {
        return tid;
    }
    let (h32, mut idx) = (((h ^ (h >> 32)) as u32) | 1, (h as usize) & H_MSK);
    while VTAB[idx].0 != 0 {
        idx = (idx + 1) & H_MSK;
    }
    let nt = N_TERMS as u32;
    N_TERMS += 1;
    VTAB[idx] = Slot(h32, nt);
    TINFO[nt as usize] = TInfo(w0, w1, off, len as u16);
    BLOOM[((h >> 32) as usize) & BLOOM_MSK] |= 1u64 << (h & 63);
    let h2 = h.rotate_left(27);
    BLOOM[((h2 >> 32) as usize) & BLOOM_MSK] |= 1u64 << (h2 & 63);
    nt
}

#[inline(always)]
unsafe fn read_tok(d: &[u8], p: &mut usize, tail: &mut [u8; 16]) -> (u64, u64, usize) {
    let mut w0 = 0u64;
    let mut len = 0usize;
    let mut m = *CMAP.get_unchecked(*d.get_unchecked(*p) as usize);
    while len < 8 {
        w0 |= (m as u64) << (len * 8);
        len += 1;
        *p += 1;
        m = *CMAP.get_unchecked(*d.get_unchecked(*p) as usize);
        if m <= 1 {
            return (w0, 0, len);
        }
    }
    let mut w1 = 0u64;
    while len < 16 {
        w1 |= (m as u64) << ((len - 8) * 8);
        len += 1;
        *p += 1;
        m = *CMAP.get_unchecked(*d.get_unchecked(*p) as usize);
        if m <= 1 {
            return (w0, w1, len);
        }
    }
    while len < 32 {
        *tail.get_unchecked_mut(len - 16) = m;
        len += 1;
        *p += 1;
        m = *CMAP.get_unchecked(*d.get_unchecked(*p) as usize);
        if m <= 1 {
            return (w0, w1, len);
        }
    }
    while m > 1 {
        *p += 1;
        len += 1;
        m = *CMAP.get_unchecked(*d.get_unchecked(*p) as usize);
    }
    (w0, w1, len)
}

#[inline(always)]
unsafe fn scan_doc<F: FnMut(usize)>(
    d_buf: &[u8],
    p: &mut usize,
    q_buf: &[u8],
    rep_map: &[u32],
    mut fn_: F,
) {
    let (mut pos, mut p_tid, mut p_pos) = (0u32, u32::MAX, u32::MAX);
    loop {
        while *CMAP.get_unchecked(*d_buf.get_unchecked(*p) as usize) == 0 {
            *p += 1;
        }
        if *d_buf.get_unchecked(*p) == b'\n' {
            *p += 1;
            break;
        }
        let mut tail = [0u8; 16];
        let (w0, w1, len) = read_tok(d_buf, p, &mut tail);
        let h = mix64(w0, w1, len);
        let h2 = h.rotate_left(27);
        let tid = if ((*BLOOM.get_unchecked(((h >> 32) as usize) & BLOOM_MSK) >> (h & 63)) & 1 != 0)
            && ((*BLOOM.get_unchecked(((h2 >> 32) as usize) & BLOOM_MSK) >> (h2 & 63)) & 1 != 0)
        {
            lookup_swar(w0, w1, &tail, len, q_buf, h)
        } else {
            u32::MAX
        };
        if tid != u32::MAX {
            let t = tid as usize;
            fn_(t);
            if p_tid == tid && pos == p_pos + 1 && t < rep_map.len() {
                let rt = *rep_map.get_unchecked(t);
                if rt != u32::MAX {
                    fn_(rt as usize);
                }
            }
            p_tid = tid;
            p_pos = pos;
        } else {
            p_tid = u32::MAX;
        }
        pos += 1;
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
fn fmt_res(cnt: u64, sum: u64, top: &[u32; 10], tc: usize, out: &mut Vec<u8>) {
    if cnt == 0 {
        out.extend_from_slice(b"0 0\n");
    } else {
        push_u64(out, cnt);
        out.push(b' ');
        push_u64(out, sum);
        for &d in &top[..tc] {
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
    close(fd);
    std::slice::from_raw_parts(p, len)
}

struct Engine<'a> {
    c_off: &'a [u32],
    c_base: &'a [u32; NUM_C],
    is_bs: &'a [bool],
    bs_idx: &'a [u32],
    doc_s: &'a [u32; NUM_C],
    doc_e: &'a [u32; NUM_C],
    w_per_doc: usize,
    g_bs: &'a [u64],
    g_u16: &'a [u16],
    term_df: &'a [u32],
    rep_map: &'a [u32],
    doc_ts: &'a [u32],
    d_buf: &'a [u8],
    q_buf: &'a [u8],
    nt: usize,
}

impl<'a> Engine<'a> {
    #[inline(always)]
    unsafe fn range(&self, c: usize, tid: usize) -> std::ops::Range<usize> {
        let base = c * self.nt;
        let s = if tid > 0 {
            *self.c_off.get_unchecked(base + tid - 1)
        } else {
            self.c_base[c]
        } as usize;
        s..*self.c_off.get_unchecked(base + tid) as usize
    }

    #[target_feature(enable = "avx2,bmi1,bmi2,lzcnt,popcnt")]
    unsafe fn exec_bool(&self, q: &Query, out: &mut Vec<u8>) {
        let (mut t_cnt, mut t_sum, mut top, mut tc) = (0u64, 0u64, [0u32; 10], 0usize);
        let mut clist = [0u16; 1024];
        let mut curr_uninit = MaybeUninit::<[u64; MAX_W]>::uninit();
        let curr = curr_uninit.assume_init_mut();

        for c in 0..NUM_C {
            let (cs, cd) = (self.doc_s[c], self.doc_e[c] - self.doc_s[c]);
            if cd == 0 {
                continue;
            }
            let cw = ((cd + 63) >> 6) as usize;
            let t0 = q.3[0] as usize;
            let mut clen = 0usize;
            let mut in_bs = self.is_bs[t0];

            let get_c_bs = |tid: usize, buf: &mut [u64; MAX_W]| {
                if tid == u32::MAX as usize {
                    buf[..cw].fill(0);
                } else if self.is_bs[tid] {
                    let off = (self.bs_idx[tid] as usize) * self.w_per_doc + ((cs >> 6) as usize);
                    buf[..cw].copy_from_slice(&self.g_bs[off..off + cw]);
                } else {
                    buf[..cw].fill(0);
                    for &ld in &self.g_u16[self.range(c, tid)] {
                        *buf.get_unchecked_mut((ld as usize) >> 6) |= 1u64 << (ld & 63);
                    }
                }
            };

            if in_bs {
                get_c_bs(t0, curr);
            } else {
                let r0 = self.range(c, t0);
                clen = r0.len();
                if clen > 1024 {
                    in_bs = true;
                    get_c_bs(t0, curr);
                } else {
                    clist[..clen].copy_from_slice(&self.g_u16[r0]);
                }
            }

            for oi in 0..(q.1 as usize - 1) {
                let op = (q.2 >> (oi * 2)) & 3;
                let nt_id = q.3[oi + 1] as usize;
                let r = self.range(c, nt_id);

                if !in_bs && op == 2 {
                    in_bs = true;
                    curr[..cw].fill(0);
                    for i in 0..clen {
                        *curr.get_unchecked_mut((clist[i] as usize) >> 6) |=
                            1u64 << (clist[i] & 63);
                    }
                }

                if in_bs {
                    if self.is_bs[nt_id] {
                        let b = &self.g_bs[(self.bs_idx[nt_id] as usize) * self.w_per_doc
                            + ((cs >> 6) as usize)..];
                        for w in 0..cw {
                            let bw = *b.get_unchecked(w);
                            let cw_ref = curr.get_unchecked_mut(w);
                            match op {
                                1 => *cw_ref &= bw,
                                2 => *cw_ref |= bw,
                                _ => *cw_ref &= !bw,
                            }
                        }
                    } else if op == 2 {
                        for &ld in &self.g_u16[r] {
                            *curr.get_unchecked_mut((ld as usize) >> 6) |= 1u64 << (ld & 63);
                        }
                    } else if op == 3 {
                        for &ld in &self.g_u16[r] {
                            *curr.get_unchecked_mut((ld as usize) >> 6) &= !(1u64 << (ld & 63));
                        }
                    } else {
                        let mut nb_uninit = MaybeUninit::<[u64; MAX_W]>::uninit();
                        let nb = nb_uninit.assume_init_mut();
                        get_c_bs(nt_id, nb);
                        for w in 0..cw {
                            *curr.get_unchecked_mut(w) &= *nb.get_unchecked(w);
                        }
                    }
                } else if op == 1 || op == 3 {
                    if clen == 0 {
                        continue;
                    }
                    let mut w = 0usize;
                    if self.is_bs[nt_id] {
                        let exp = (op == 1) as u64;
                        let b = &self.g_bs[(self.bs_idx[nt_id] as usize) * self.w_per_doc..];
                        for i in 0..clen {
                            let d = (cs + clist[i] as u32) as usize;
                            if ((*b.get_unchecked(d >> 6) >> (d & 63)) & 1) == exp {
                                clist[w] = clist[i];
                                w += 1;
                            }
                        }
                    } else {
                        let (mut i, mut j) = (0usize, r.start);
                        while i < clen && j < r.end {
                            let (cv, gv) = (clist[i], self.g_u16[j]);
                            if (op == 1 && cv == gv) || (op == 3 && cv < gv) {
                                clist[w] = cv;
                                w += 1;
                            }
                            i += (cv <= gv) as usize;
                            j += (cv >= gv) as usize;
                        }
                        if op == 3 {
                            clist.copy_within(i..clen, w);
                            w += clen - i;
                        }
                    }
                    clen = w;
                }
            }

            let mut add_res = |d: u32| {
                t_cnt += 1;
                t_sum += d as u64;
                if tc < 10 {
                    top[tc] = d;
                    tc += 1;
                }
            };

            if !in_bs {
                for i in 0..clen {
                    add_res(cs + clist[i] as u32);
                }
            } else {
                if (cd & 63) != 0 {
                    *curr.get_unchecked_mut(cw - 1) &= (1u64 << (cd & 63)) - 1;
                }
                for w in 0..cw {
                    let mut v = *curr.get_unchecked(w);
                    let base = cs + ((w as u32) << 6);
                    while v != 0 {
                        add_res(base + v.trailing_zeros());
                        v &= v - 1;
                    }
                }
            }
        }
        fmt_res(t_cnt, t_sum, &top, tc, out);
    }

    #[target_feature(enable = "avx2,bmi1,bmi2,lzcnt,popcnt")]
    unsafe fn exec_phrase(&self, q: &Query, out: &mut Vec<u8>) {
        let (k, mut nd, mut dist) = (q.1 as usize, 0usize, [0usize; 16]);
        let mut add_dist = |t: usize| {
            if t == u32::MAX as usize || *self.term_df.get_unchecked(t) == 0 {
                false
            } else {
                if !dist[..nd].contains(&t) {
                    dist[nd] = t;
                    nd += 1;
                }
                true
            }
        };

        for i in 0..k {
            let t = q.3[i] as usize;
            if !add_dist(t) {
                return fmt_res(0, 0, &[0; 10], 0, out);
            }
            if i + 1 < k && t == q.3[i + 1] as usize && t < self.rep_map.len() {
                let rt = *self.rep_map.get_unchecked(t) as usize;
                if rt != u32::MAX as usize && !add_dist(rt) {
                    return fmt_res(0, 0, &[0; 10], 0, out);
                }
            }
        }
        dist[..nd].sort_unstable_by_key(|&t| *self.term_df.get_unchecked(t));
        let mut pt = [TInfo::default(); 8];
        for i in 0..k {
            pt[i] = *TINFO.get_unchecked(q.3[i] as usize);
        }

        let check_match = |j: usize, l: usize, w0: u64, w1: u64, tail: &[u8]| -> bool {
            l == pt[j].3 as usize
                && w0 == pt[j].0
                && w1 == pt[j].1
                && (l <= 16
                    || &self.q_buf[pt[j].2 as usize + 16..pt[j].2 as usize + l] == &tail[..l - 16])
        };

        let (mut t_cnt, mut t_sum, mut top, mut tc) = (0u64, 0u64, [0u32; 10], 0usize);
        let mut cand_uninit = MaybeUninit::<[u16; 21000]>::uninit();
        let cand = cand_uninit.assume_init_mut();

        for c in 0..NUM_C {
            let (cs, cd) = (self.doc_s[c], self.doc_e[c] - self.doc_s[c]);
            if cd == 0 {
                continue;
            }
            let (rare, mut clen) = (dist[0], 0usize);
            if !self.is_bs[rare] {
                let r = self.range(c, rare);
                clen = r.len();
                if clen == 0 {
                    continue;
                }
                cand[..clen].copy_from_slice(&self.g_u16[r]);
            } else {
                let cw = ((cd + 63) >> 6) as usize;
                let src = &self.g_bs
                    [(self.bs_idx[rare] as usize) * self.w_per_doc + ((cs >> 6) as usize)..];
                for w in 0..cw {
                    let mut v = *src.get_unchecked(w);
                    while v != 0 {
                        let id = (w << 6) + v.trailing_zeros() as usize;
                        if id < cd as usize {
                            cand[clen] = id as u16;
                            clen += 1;
                        }
                        v &= v - 1;
                    }
                }
                if clen == 0 {
                    continue;
                }
            }

            for &ot in &dist[1..nd] {
                if clen == 0 {
                    break;
                }
                let mut w = 0usize;
                if self.is_bs[ot] {
                    let b = &self.g_bs[(self.bs_idx[ot] as usize) * self.w_per_doc..];
                    for i in 0..clen {
                        let d = (cs + cand[i] as u32) as usize;
                        if ((*b.get_unchecked(d >> 6) >> (d & 63)) & 1) != 0 {
                            cand[w] = cand[i];
                            w += 1;
                        }
                    }
                } else {
                    let r = self.range(c, ot);
                    let (mut i, mut j) = (0usize, r.start);
                    while i < clen && j < r.end {
                        let (cv, gv) = (cand[i], self.g_u16[j]);
                        if cv == gv {
                            cand[w] = cv;
                            w += 1;
                        }
                        i += (cv <= gv) as usize;
                        j += (cv >= gv) as usize;
                    }
                }
                clen = w;
            }

            for ci in 0..clen {
                let d = cs + cand[ci] as u32;
                let mut sp = *self.doc_ts.get_unchecked(d as usize) as usize;
                let mut matched = false;
                loop {
                    while *CMAP.get_unchecked(*self.d_buf.get_unchecked(sp) as usize) == 0 {
                        sp += 1;
                    }
                    if *self.d_buf.get_unchecked(sp) == b'\n' {
                        break;
                    }
                    let mut tail = [0u8; 16];
                    let (w0, w1, l) = read_tok(self.d_buf, &mut sp, &mut tail);
                    if check_match(0, l, w0, w1, &tail) {
                        let (mut xp, mut j) = (sp, 1usize);
                        while j < k {
                            while *CMAP.get_unchecked(*self.d_buf.get_unchecked(xp) as usize) == 0 {
                                xp += 1;
                            }
                            if *self.d_buf.get_unchecked(xp) == b'\n' {
                                break;
                            }
                            let mut jtail = [0u8; 16];
                            let (jw0, jw1, jl) = read_tok(self.d_buf, &mut xp, &mut jtail);
                            if !check_match(j, jl, jw0, jw1, &jtail) {
                                break;
                            }
                            j += 1;
                        }
                        if j == k {
                            matched = true;
                            break;
                        }
                    }
                }
                if matched {
                    t_cnt += 1;
                    t_sum += d as u64;
                    if tc < 10 {
                        top[tc] = d;
                        tc += 1;
                    }
                }
            }
        }
        fmt_res(t_cnt, t_sum, &top, tc, out);
    }
}

fn main() {
    unsafe {
        mallopt(-8, 2);
        mallopt(-4, 0);

        let mut args = std::env::args().skip(1);
        let (mut d_dir, mut o_dir) = ("/data".into(), "/out".into());
        while let Some(a) = args.next() {
            match a.as_str() {
                "--data" => d_dir = args.next().unwrap(),
                "--out" => o_dir = args.next().unwrap(),
                _ => {}
            }
        }

        let q_buf = map_file(&format!("{}/queries.txt", d_dir));
        let d_buf = map_file(&format!("{}/docs.txt", d_dir));

        let mut queries = Vec::with_capacity(65536);
        let (mut qp, qe) = (0usize, q_buf.len());
        while qp < qe {
            while qp < qe && q_buf[qp] <= b' ' {
                qp += 1;
            }
            if qp >= qe {
                break;
            }
            let ib = q_buf[qp..].starts_with(b"BOOL ");
            qp += if ib { 5 } else { 8 };
            let mut le = qp;
            while le < qe && q_buf[le] != if ib { b'\n' } else { b'"' } {
                le += 1;
            }
            let mut q = Query((!ib) as u8, 0, 0, [0; 8]);
            for tok in q_buf[qp..le]
                .split(|&b| b == b' ')
                .filter(|s| !s.is_empty())
            {
                let tk = (tok.as_ptr() as usize) - (q_buf.as_ptr() as usize);
                match (ib, tok) {
                    (true, b"AND") => q.2 |= 1 << ((q.1 - 1) * 2),
                    (true, b"OR") => q.2 |= 2 << ((q.1 - 1) * 2),
                    (true, b"NOT") => q.2 |= 3 << ((q.1 - 1) * 2),
                    _ => {
                        q.3[q.1 as usize] = insert_term(tok, tk as u32, q_buf);
                        q.1 += 1;
                    }
                }
            }
            queries.push(q);
            qp = le;
            while qp < qe && q_buf[qp] != b'\n' {
                qp += 1;
            }
            qp += 1;
        }

        let mut rep_map = vec![u32::MAX; N_TERMS];
        for q in &queries {
            if q.0 == 1 {
                for w in q.3[..q.1 as usize].windows(2) {
                    if w[0] == w[1] && rep_map[w[0] as usize] == u32::MAX {
                        let (t, rt) = (w[0] as usize, N_TERMS as u32);
                        N_TERMS += 1;
                        rep_map[t] = rt;
                        TINFO[rt as usize] = TINFO[t];
                    }
                }
            }
        }

        let f_sz = d_buf.len();
        let mut pl = f_sz - 1;
        while pl > 0 && d_buf[pl - 1] != b'\n' {
            pl -= 1;
        }
        let mut total_docs = 0u32;
        while pl < f_sz && d_buf[pl] >= b'0' && d_buf[pl] <= b'9' {
            total_docs = total_docs * 10 + (d_buf[pl] - b'0') as u32;
            pl += 1;
        }
        total_docs += 1;

        let mut doc_ts = vec![0u32; total_docs as usize];
        let dpc = (total_docs + NUM_C as u32 - 1) / NUM_C as u32;
        let (mut doc_s, mut doc_e, mut c_byte) =
            ([0u32; NUM_C], [0u32; NUM_C], [0usize; NUM_C + 1]);
        c_byte[NUM_C] = f_sz;
        for c in 0..NUM_C {
            doc_s[c] = total_docs.min(if c > 0 {
                (c as u32 * dpc + 63) & !63
            } else {
                0
            });
            if c > 0 {
                doc_e[c - 1] = doc_s[c];
            }
        }
        doc_e[NUM_C - 1] = total_docs;
        for c in 1..NUM_C {
            let (mut lo, mut hi) = (0, f_sz);
            while lo < hi {
                let mut p = (lo + hi) / 2;
                while p > 0 && d_buf[p - 1] != b'\n' {
                    p += 1;
                }
                let (mut id, mut cp) = (0u32, p);
                while cp < f_sz && d_buf[cp] >= b'0' && d_buf[cp] <= b'9' {
                    id = id * 10 + (d_buf[cp] - b'0') as u32;
                    cp += 1;
                }
                if p >= f_sz || id >= doc_s[c] {
                    c_byte[c] = p;
                    hi = (lo + hi) / 2;
                } else {
                    lo = p + 1;
                }
            }
        }

        let nt = N_TERMS;
        let mut sc = vec![0u32; NUM_C * nt];
        let doc_ts_ptr = doc_ts.as_mut_ptr() as usize;

        std::thread::scope(|s| {
            for (c, sc_c) in sc.chunks_mut(nt).enumerate() {
                let (r_ref, ds, cb) = (&rep_map, &doc_s, &c_byte);
                std::thread::Builder::new()
                    .stack_size(131072)
                    .spawn_scoped(s, move || {
                        let mut seen = vec![u16::MAX; nt];
                        let (mut p, pe, d_ts, mut d, mut ld) =
                            (cb[c], cb[c + 1], doc_ts_ptr as *mut u32, ds[c], 0u16);
                        while p < pe {
                            while *d_buf.get_unchecked(p) != b'\t' {
                                p += 1;
                            }
                            p += 1;
                            *d_ts.add(d as usize) = p as u32;
                            scan_doc(d_buf, &mut p, q_buf, r_ref, |t| {
                                if *seen.get_unchecked(t) != ld {
                                    *seen.get_unchecked_mut(t) = ld;
                                    *sc_c.get_unchecked_mut(t) += 1;
                                }
                            });
                            d += 1;
                            ld += 1;
                        }
                    })
                    .unwrap();
            }
        });

        let mut term_df = vec![0u32; nt];
        for c in 0..NUM_C {
            for i in 0..nt {
                term_df[i] += sc[c * nt + i];
            }
        }

        let (mut is_bs, mut bs_idx, mut n_bs) = (vec![false; nt], vec![u32::MAX; nt], 0usize);
        for i in 0..nt {
            if term_df[i] > BS_TH {
                is_bs[i] = true;
                bs_idx[i] = n_bs as u32;
                n_bs += 1;
            }
        }

        let w_per_doc = ((total_docs + 63) >> 6) as usize;
        let mut tot_u16 = 0u32;
        let mut c_base = [0u32; NUM_C];
        let mut c_off = sc;

        for c in 0..NUM_C {
            c_base[c] = tot_u16;
            let c_base_off = c * nt;
            for i in 0..nt {
                let cnt = c_off[c_base_off + i];
                c_off[c_base_off + i] = tot_u16;
                if !is_bs[i] {
                    tot_u16 += cnt;
                }
            }
        }

        let mut g_bs = vec![0u64; n_bs * w_per_doc];
        let mut g_u16 = vec![0u16; tot_u16 as usize];
        let g_bs_ptr = g_bs.as_mut_ptr() as usize;
        let g_u16_ptr = g_u16.as_mut_ptr() as usize;
        let c_off_ptr = c_off.as_mut_ptr() as usize;

        std::thread::scope(|s| {
            for c in 0..NUM_C {
                let (ib, bi, ds, cb, r_ref) = (&is_bs, &bs_idx, &doc_s, &c_byte, &rep_map);
                std::thread::Builder::new()
                    .stack_size(131072)
                    .spawn_scoped(s, move || {
                        let bs_ptr = g_bs_ptr as *mut u64;
                        let u16_ptr = g_u16_ptr as *mut u16;
                        let co_ptr = (c_off_ptr as *mut u32).add(c * nt);
                        let mut seen = vec![u16::MAX; nt];
                        let (mut p, pe, mut d, mut ld) = (cb[c], cb[c + 1], ds[c], 0u16);

                        while p < pe {
                            while *d_buf.get_unchecked(p) != b'\t' {
                                p += 1;
                            }
                            p += 1;
                            scan_doc(d_buf, &mut p, q_buf, r_ref, |t| {
                                if *seen.get_unchecked(t) != ld {
                                    *seen.get_unchecked_mut(t) = ld;
                                    if *ib.get_unchecked(t) {
                                        *bs_ptr.add(
                                            (*bi.get_unchecked(t) as usize) * w_per_doc
                                                + ((d >> 6) as usize),
                                        ) |= 1u64 << (d & 63);
                                    } else {
                                        let cur = *co_ptr.add(t);
                                        *u16_ptr.add(cur as usize) = ld;
                                        *co_ptr.add(t) = cur + 1;
                                    }
                                }
                            });
                            d += 1;
                            ld += 1;
                        }
                    })
                    .unwrap();
            }
        });

        let eng = Engine {
            c_off: &c_off,
            c_base: &c_base,
            is_bs: &is_bs,
            bs_idx: &bs_idx,
            doc_s: &doc_s,
            doc_e: &doc_e,
            w_per_doc,
            g_bs: &g_bs,
            g_u16: &g_u16,
            term_df: &term_df,
            rep_map: &rep_map,
            doc_ts: &doc_ts,
            d_buf,
            q_buf,
            nt,
        };

        let chunk_q = (queries.len() + NUM_C - 1) / NUM_C;
        let mut out_bufs = vec![Vec::<u8>::with_capacity(chunk_q * 48); NUM_C];

        std::thread::scope(|s| {
            for (qc, ob) in queries.chunks(chunk_q).zip(&mut out_bufs) {
                let eng_ref = &eng;
                std::thread::Builder::new()
                    .stack_size(131072)
                    .spawn_scoped(s, move || {
                        for q in qc {
                            if q.0 == 0 {
                                eng_ref.exec_bool(q, ob);
                            } else {
                                eng_ref.exec_phrase(q, ob);
                            }
                        }
                    })
                    .unwrap();
            }
        });

        drop(g_bs);
        drop(g_u16);
        drop(c_off);
        drop(doc_ts);
        drop(queries);

        let mut out_file = File::create(format!("{}/results.txt", o_dir)).unwrap();
        for ob in &out_bufs {
            out_file.write_all(ob).unwrap();
        }
    }
}
