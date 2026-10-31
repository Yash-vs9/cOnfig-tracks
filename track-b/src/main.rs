#![allow(static_mut_refs)]
use std::{arch::x86_64::*, fs::File, io::Write, mem::MaybeUninit, os::unix::io::IntoRawFd, ptr, sync::atomic::{AtomicUsize, Ordering}};

extern "C" {
    fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
    fn munmap(a: *mut u8, l: usize) -> i32;
    fn close(fd: i32) -> i32;
    fn mallopt(p: i32, v: i32) -> i32;
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
}

const H_SZ: usize = 131072;
const H_MSK: usize = H_SZ - 1;
const NUM_C: usize = 12;

#[derive(Clone, Copy, Default)]
#[repr(C, align(16))]
struct Slot { w0: u64, h_tag: u32, tid: u32 }

#[derive(Clone, Copy, Default)]
struct TInfo(u64, u32, u16, u16);

#[derive(Clone, Copy)]
struct Query { ib: u8, k: u8, ops: u16, t_idx: u32 }

const CMAP: [u8; 256] = {
    let mut m = [0u8; 256];
    m[b'\n' as usize] = 1;
    let mut i = 0u8;
    loop {
        if i >= b'0' && i <= b'9' { m[i as usize] = i; }
        if i >= b'a' && i <= b'z' { m[i as usize] = i; m[(i - 32) as usize] = i; }
        if i == 255 { break; }
        i += 1;
    }
    m
};

static mut VTAB: [Slot; H_SZ] = [Slot { w0: 0, h_tag: 0, tid: 0 }; H_SZ];
static mut TINFO: [TInfo; 81920] = [TInfo(0, 0, 0, 0); 81920];
static mut N_TERMS: usize = 0;

#[inline(always)]
fn mix64(w0: u64, w1: u64, len: usize) -> u64 {
    let k = if len <= 8 { len as u64 } else { w1.wrapping_mul(0x9e3779b97f4a7c15).wrapping_add(len as u64) };
    let h = (w0 ^ k).wrapping_mul(0xbf58476d1ce4e5b9);
    h ^ (h >> 31)
}

#[inline(always)]
unsafe fn lookup(w0: u64, w1: u64, tail: &[u8], len: usize, q_buf: &[u8], h: u64) -> u32 {
    let (h32, mut idx) = (((h ^ (h >> 32)) as u32) | 1, (h as usize) & H_MSK);
    let h_tag = (h32 & 0x00FFFFFF) | ((len as u32) << 24);
    loop {
        let s = VTAB.get_unchecked(idx);
        if s.h_tag == 0 { return u32::MAX; }
        if s.h_tag == h_tag && s.w0 == w0 {
            if len <= 8 { return s.tid; }
            let ti = *TINFO.get_unchecked(s.tid as usize);
            let q_ptr = q_buf.as_ptr().add(ti.1 as usize + 8);
            let off = len - 8;
            let w1_b = w1.to_ne_bytes();
            let match_w1 = if off <= 8 {
                std::slice::from_raw_parts(q_ptr, off) == &w1_b[..off]
            } else {
                std::slice::from_raw_parts(q_ptr, 8) == &w1_b[..8]
                    && std::slice::from_raw_parts(q_ptr.add(8), off - 8) == &tail[..off - 8]
            };
            if match_w1 { return s.tid; }
        }
        idx = (idx + 1) & H_MSK;
    }
}

unsafe fn insert_term(s: &[u8], off: u32, q_buf: &[u8]) -> u32 {
    let (mut w0, mut w1, len) = (0u64, 0u64, s.len());
    ptr::copy_nonoverlapping(s.as_ptr(), &mut w0 as *mut _ as *mut u8, len.min(8));
    if len > 8 { ptr::copy_nonoverlapping(s.as_ptr().add(8), &mut w1 as *mut _ as *mut u8, (len - 8).min(8)); }
    let h = mix64(w0, w1, len);
    let tail = if len > 16 { &s[16..] } else { &[] };
    let tid = lookup(w0, w1, tail, len, q_buf, h);
    if tid != u32::MAX { return tid; }
    let (h32, mut idx) = (((h ^ (h >> 32)) as u32) | 1, (h as usize) & H_MSK);
    let h_tag = (h32 & 0x00FFFFFF) | ((len as u32) << 24);
    while VTAB[idx].h_tag != 0 { idx = (idx + 1) & H_MSK; }
    let nt = N_TERMS as u32;
    N_TERMS += 1;
    VTAB[idx] = Slot { w0, h_tag, tid: nt };
    TINFO[nt as usize] = TInfo(w0, off, len as u16, 0);
    nt
}

#[inline(always)]
unsafe fn read_tok(d: &[u8], p: &mut usize, tail: &mut [u8; 16]) -> (u64, u64, usize) {
    let mut m = *CMAP.get_unchecked(*d.get_unchecked(*p) as usize);
    let mut w0 = m as u64; *p += 1;
    m = *CMAP.get_unchecked(*d.get_unchecked(*p) as usize);
    if m <= 1 { return (w0, 0, 1); }
    w0 |= (m as u64) << 8; *p += 1;
    m = *CMAP.get_unchecked(*d.get_unchecked(*p) as usize);
    if m <= 1 { return (w0, 0, 2); }
    w0 |= (m as u64) << 16; *p += 1;
    m = *CMAP.get_unchecked(*d.get_unchecked(*p) as usize);
    if m <= 1 { return (w0, 0, 3); }
    w0 |= (m as u64) << 24; *p += 1;
    m = *CMAP.get_unchecked(*d.get_unchecked(*p) as usize);
    if m <= 1 { return (w0, 0, 4); }
    w0 |= (m as u64) << 32; *p += 1;
    m = *CMAP.get_unchecked(*d.get_unchecked(*p) as usize);
    if m <= 1 { return (w0, 0, 5); }
    w0 |= (m as u64) << 40; *p += 1;
    m = *CMAP.get_unchecked(*d.get_unchecked(*p) as usize);
    if m <= 1 { return (w0, 0, 6); }
    w0 |= (m as u64) << 48; *p += 1;
    m = *CMAP.get_unchecked(*d.get_unchecked(*p) as usize);
    if m <= 1 { return (w0, 0, 7); }
    w0 |= (m as u64) << 56; *p += 1;
    m = *CMAP.get_unchecked(*d.get_unchecked(*p) as usize);
    if m <= 1 { return (w0, 0, 8); }

    let mut w1 = m as u64;
    let mut len = 9usize; *p += 1;
    m = *CMAP.get_unchecked(*d.get_unchecked(*p) as usize);
    while len < 16 {
        if m <= 1 { return (w0, w1, len); }
        w1 |= (m as u64) << ((len - 8) * 8); len += 1; *p += 1;
        m = *CMAP.get_unchecked(*d.get_unchecked(*p) as usize);
    }
    while len < 32 {
        if m <= 1 { return (w0, w1, len); }
        *tail.get_unchecked_mut(len - 16) = m; len += 1; *p += 1;
        m = *CMAP.get_unchecked(*d.get_unchecked(*p) as usize);
    }
    while m > 1 { *p += 1; len += 1; m = *CMAP.get_unchecked(*d.get_unchecked(*p) as usize); }
    (w0, w1, len)
}

#[target_feature(enable = "avx2,bmi1,bmi2,lzcnt,popcnt")]
#[inline]
unsafe fn scan_doc<F: FnMut(usize)>(d_buf: &[u8], p: &mut usize, q_buf: &[u8], rep_map: &[u32], mut fn_: F) {
    let (mut pos, mut p_tid, mut p_pos) = (0u32, u32::MAX, u32::MAX);
    loop {
        while *CMAP.get_unchecked(*d_buf.get_unchecked(*p) as usize) == 0 { *p += 1; }
        if *d_buf.get_unchecked(*p) == b'\n' { *p += 1; break; }
        let mut tail = [0u8; 16];
        let (w0, w1, len) = read_tok(d_buf, p, &mut tail);
        let h = mix64(w0, w1, len);
        let tid = lookup(w0, w1, &tail, len, q_buf, h);
        if tid != u32::MAX {
            let t = tid as usize;
            fn_(t);
            if p_tid == tid && pos == p_pos + 1 && t < rep_map.len() {
                let rt = *rep_map.get_unchecked(t);
                if rt != u32::MAX { fn_(rt as usize); }
            }
            p_tid = tid; p_pos = pos;
        } else { p_tid = u32::MAX; }
        pos += 1;
    }
}

#[inline(always)]
fn push_u64(buf: &mut Vec<u8>, mut n: u64) {
    let (mut tmp, mut i) = ([0u8; 20], 20);
    loop {
        i -= 1; tmp[i] = b'0' + (n % 10) as u8; n /= 10;
        if n == 0 { break; }
    }
    buf.extend_from_slice(&tmp[i..]);
}

#[inline(always)]
fn fmt_res(cnt: u64, sum: u64, top: &[u32; 10], tc: usize, out: &mut Vec<u8>) {
    if cnt == 0 { out.extend_from_slice(b"0 0\n"); } else {
        push_u64(out, cnt); out.push(b' '); push_u64(out, sum);
        for &d in &top[..tc] { out.push(b' '); push_u64(out, d as u64); }
        out.push(b'\n');
    }
}

#[inline(always)]
fn record_doc(d: u32, cnt: &mut u64, sum: &mut u64, top: &mut [u32; 10], tc: &mut usize) {
    *cnt += 1; *sum += d as u64;
    if *tc < 10 { top[*tc] = d; *tc += 1; }
}

unsafe fn map_file(path: &str) -> &'static [u8] {
    let f = File::open(path).unwrap();
    let len = f.metadata().unwrap().len() as usize;
    let fd = f.into_raw_fd();
    let p = mmap(ptr::null_mut(), len, 1, 1, fd, 0);
    close(fd);
    std::slice::from_raw_parts(p, len)
}

unsafe fn alloc_mmap<T>(count: usize) -> *mut T { mmap(ptr::null_mut(), count * size_of::<T>(), 3, 0x22, -1, 0) as *mut T }
unsafe fn free_mmap<T>(p: *mut T, count: usize) { munmap(p as *mut u8, count * size_of::<T>()); }

struct Engine<'a> {
    c_off: &'a [u32],
    c_base: &'a [u32; NUM_C],
    bs_idx: &'a [u16],
    doc_s: &'a [u32; NUM_C],
    doc_e: &'a [u32; NUM_C],
    w_per_doc: usize,
    g_bs: &'a [u64],
    g_u16: &'a [u16],
    rep_map: &'a [u32],
    doc_ts: &'a [u32],
    d_buf: &'a [u8],
    q_buf: &'a [u8],
    q_terms: &'a [u32],
    nt: usize,
}

impl<'a> Engine<'a> {
    #[inline(always)]
    unsafe fn range(&self, c: usize, tid: usize) -> std::ops::Range<usize> {
        let base = c * self.nt;
        let s = if tid > 0 { *self.c_off.get_unchecked(base + tid - 1) } else { self.c_base[c] } as usize;
        s..*self.c_off.get_unchecked(base + tid) as usize
    }

    #[inline(always)]
    unsafe fn is_bs(&self, tid: usize) -> bool { *self.bs_idx.get_unchecked(tid) != u16::MAX }

    #[inline(always)]
    unsafe fn intersect_and(&self, c: usize, t: usize, cs: u32, list: &mut [u16], len: usize) -> usize {
        if len == 0 { return 0; }
        let mut w = 0usize;
        if self.is_bs(t) {
            let b = &self.g_bs[(self.bs_idx[t] as usize) * self.w_per_doc..];
            for i in 0..len {
                let d = (cs + list[i] as u32) as usize;
                if ((*b.get_unchecked(d >> 6) >> (d & 63)) & 1) != 0 { list[w] = list[i]; w += 1; }
            }
        } else {
            let r = self.range(c, t);
            let (mut i, mut j) = (0usize, r.start);
            while i < len && j < r.end {
                let (cv, gv) = (list[i], self.g_u16[j]);
                if cv == gv { list[w] = cv; w += 1; }
                i += (cv <= gv) as usize; j += (cv >= gv) as usize;
            }
        }
        w
    }

    #[inline(always)]
    unsafe fn intersect_not(&self, c: usize, t: usize, cs: u32, list: &mut [u16], len: usize) -> usize {
        if len == 0 { return 0; }
        let mut w = 0usize;
        if self.is_bs(t) {
            let b = &self.g_bs[(self.bs_idx[t] as usize) * self.w_per_doc..];
            for i in 0..len {
                let d = (cs + list[i] as u32) as usize;
                if ((*b.get_unchecked(d >> 6) >> (d & 63)) & 1) == 0 { list[w] = list[i]; w += 1; }
            }
        } else {
            let r = self.range(c, t);
            let (mut i, mut j) = (0usize, r.start);
            while i < len && j < r.end {
                let (cv, gv) = (list[i], self.g_u16[j]);
                if cv < gv { list[w] = cv; w += 1; }
                i += (cv <= gv) as usize; j += (cv >= gv) as usize;
            }
            list.copy_within(i..len, w);
            w += len - i;
        }
        w
    }

    #[target_feature(enable = "avx2,bmi1,bmi2,lzcnt,popcnt")]
    unsafe fn emit_bits(&self, curr: &[u64], cs: u32, cd: u32, cw: usize, t_cnt: &mut u64, t_sum: &mut u64, top: &mut [u32; 10], tc: &mut usize) {
        let mut w = 0usize;
        while w + 4 <= cw {
            let v4 = _mm256_loadu_si256(curr.as_ptr().add(w) as *const __m256i);
            if _mm256_testz_si256(v4, v4) != 0 { w += 4; continue; }
            for i in 0..4 {
                let mut v = *curr.get_unchecked(w + i);
                if (w + i == cw - 1) && ((cd & 63) != 0) { v &= (1u64 << (cd & 63)) - 1; }
                let base = cs + (((w + i) as u32) << 6);
                while v != 0 { record_doc(base + v.trailing_zeros(), t_cnt, t_sum, top, tc); v &= v - 1; }
            }
            w += 4;
        }
        while w < cw {
            let mut v = *curr.get_unchecked(w);
            if (w == cw - 1) && ((cd & 63) != 0) { v &= (1u64 << (cd & 63)) - 1; }
            let base = cs + ((w as u32) << 6);
            while v != 0 { record_doc(base + v.trailing_zeros(), t_cnt, t_sum, top, tc); v &= v - 1; }
            w += 1;
        }
    }

    #[target_feature(enable = "avx2,bmi1,bmi2,lzcnt,popcnt")]
    unsafe fn exec_bool(&self, q: &Query, out: &mut Vec<u8>) {
        let (mut t_cnt, mut t_sum, mut top, mut tc) = (0u64, 0u64, [0u32; 10], 0usize);
        let mut curr = [0u64; 384];
        let mut clist = [0u16; 21000];
        let q_ts = &self.q_terms[q.t_idx as usize..q.t_idx as usize + q.k as usize];
        let is_pure_and = (0..q.k - 1).all(|i| ((q.ops >> (i * 2)) & 3) == 1);

        for c in 0..NUM_C {
            let (cs, cd) = (self.doc_s[c], self.doc_e[c] - self.doc_s[c]);
            if cd == 0 { continue; }
            let cw = ((cd + 63) >> 6) as usize;
            let mut clen = 0usize;
            let mut in_bs = false;

            if is_pure_and {
                let (mut min_len, mut min_t, mut zero) = (usize::MAX, usize::MAX, false);
                for &tid in q_ts {
                    let t = tid as usize;
                    if !self.is_bs(t) {
                        let l = self.range(c, t).len();
                        if l == 0 { zero = true; break; }
                        if l < min_len { min_len = l; min_t = t; }
                    }
                }
                if zero { continue; }
                if min_t != usize::MAX {
                    let r0 = self.range(c, min_t);
                    clen = r0.len();
                    clist[..clen].copy_from_slice(&self.g_u16[r0]);
                    for &tid in q_ts {
                        let t = tid as usize;
                        if t != min_t {
                            clen = self.intersect_and(c, t, cs, &mut clist, clen);
                            if clen == 0 { break; }
                        }
                    }
                    for &ld in &clist[..clen] { record_doc(cs + ld as u32, &mut t_cnt, &mut t_sum, &mut top, &mut tc); }
                    continue;
                }
            }

            let t0 = q_ts[0] as usize;
            if self.is_bs(t0) {
                in_bs = true;
                let src = &self.g_bs[(self.bs_idx[t0] as usize) * self.w_per_doc + ((cs >> 6) as usize)..];
                curr[..cw].copy_from_slice(&src[..cw]);
            } else {
                let r = self.range(c, t0);
                clen = r.len();
                clist[..clen].copy_from_slice(&self.g_u16[r]);
            }

            for i in 0..(q.k - 1) as usize {
                let op = ((q.ops >> (i * 2)) & 3) as u8;
                let nt_id = q_ts[i + 1] as usize;
                if !in_bs && op == 2 {
                    curr[..cw].fill(0);
                    for &ld in &clist[..clen] { *curr.get_unchecked_mut((ld as usize) >> 6) |= 1u64 << (ld & 63); }
                    in_bs = true;
                }
                if in_bs {
                    if self.is_bs(nt_id) {
                        let src = &self.g_bs[(self.bs_idx[nt_id] as usize) * self.w_per_doc + ((cs >> 6) as usize)..];
                        let mut w = 0usize;
                        while w + 4 <= cw {
                            let (c_v, s_v) = (_mm256_loadu_si256(curr.as_ptr().add(w) as *const __m256i), _mm256_loadu_si256(src.as_ptr().add(w) as *const __m256i));
                            let res = match op { 1 => _mm256_and_si256(c_v, s_v), 2 => _mm256_or_si256(c_v, s_v), _ => _mm256_andnot_si256(s_v, c_v) };
                            _mm256_storeu_si256(curr.as_mut_ptr().add(w) as *mut __m256i, res);
                            w += 4;
                        }
                        while w < cw {
                            match op { 1 => *curr.get_unchecked_mut(w) &= *src.get_unchecked(w), 2 => *curr.get_unchecked_mut(w) |= *src.get_unchecked(w), _ => *curr.get_unchecked_mut(w) &= !*src.get_unchecked(w) }
                            w += 1;
                        }
                    } else if op == 2 {
                        for &ld in &self.g_u16[self.range(c, nt_id)] { *curr.get_unchecked_mut((ld as usize) >> 6) |= 1u64 << (ld & 63); }
                    } else if op == 3 {
                        for &ld in &self.g_u16[self.range(c, nt_id)] { *curr.get_unchecked_mut((ld as usize) >> 6) &= !(1u64 << (ld & 63)); }
                    } else {
                        let mut w = 0usize;
                        for &ld in &self.g_u16[self.range(c, nt_id)] {
                            if (*curr.get_unchecked((ld as usize) >> 6) & (1u64 << (ld & 63))) != 0 { clist[w] = ld; w += 1; }
                        }
                        clen = w; in_bs = false;
                    }
                } else if op == 1 { clen = self.intersect_and(c, nt_id, cs, &mut clist, clen); }
                else if op == 3 { clen = self.intersect_not(c, nt_id, cs, &mut clist, clen); }
            }

            if !in_bs {
                for &ld in &clist[..clen] { record_doc(cs + ld as u32, &mut t_cnt, &mut t_sum, &mut top, &mut tc); }
            } else {
                self.emit_bits(&curr, cs, cd, cw, &mut t_cnt, &mut t_sum, &mut top, &mut tc);
            }
        }
        fmt_res(t_cnt, t_sum, &top, tc, out);
    }

    #[target_feature(enable = "avx2,bmi1,bmi2,lzcnt,popcnt")]
    unsafe fn exec_phrase(&self, q: &Query, out: &mut Vec<u8>) {
        let (k, mut nd, mut dist) = (q.k as usize, 0usize, [0usize; 16]);
        let q_ts = &self.q_terms[q.t_idx as usize..q.t_idx as usize + k];
        let mut add_dist = |t: usize| {
            if t == u32::MAX as usize || TINFO[t].3 == 0 { false } else {
                if !dist[..nd].contains(&t) { dist[nd] = t; nd += 1; }
                true
            }
        };

        for i in 0..k {
            let t = q_ts[i] as usize;
            if !add_dist(t) { return fmt_res(0, 0, &[0; 10], 0, out); }
            if i + 1 < k && t == q_ts[i + 1] as usize && t < self.rep_map.len() {
                let rt = *self.rep_map.get_unchecked(t) as usize;
                if rt != u32::MAX as usize && !add_dist(rt) { return fmt_res(0, 0, &[0; 10], 0, out); }
            }
        }
        dist[..nd].sort_unstable_by_key(|&t| TINFO[t].3);

        let (mut pw0, mut pw1, mut plen, mut poff) = ([0u64; 8], [0u64; 8], [0usize; 8], [0usize; 8]);
        for i in 0..k {
            let ti = *TINFO.get_unchecked(q_ts[i] as usize);
            pw0[i] = ti.0; plen[i] = ti.2 as usize; poff[i] = ti.1 as usize;
            if plen[i] > 8 {
                let mut w1 = 0u64;
                ptr::copy_nonoverlapping(self.q_buf.as_ptr().add(ti.1 as usize + 8), &mut w1 as *mut _ as *mut u8, (plen[i] - 8).min(8));
                pw1[i] = w1;
            }
        }

        let check_match = |j: usize, l: usize, w0: u64, w1: u64, tail: &[u8]| -> bool {
            l == plen[j] && w0 == pw0[j] && (l <= 8 || (w1 == pw1[j] && (l <= 16 || &self.q_buf[poff[j] + 16..poff[j] + l] == &tail[..l - 16])))
        };

        let first_ch = (pw0[0] & 0xFF) as u8;
        let first_up = if first_ch >= b'a' && first_ch <= b'z' { first_ch - 32 } else { first_ch };
        let (v_lo, v_up, v_nl) = (_mm256_set1_epi8(first_ch as i8), _mm256_set1_epi8(first_up as i8), _mm256_set1_epi8(b'\n' as i8));

        let (mut t_cnt, mut t_sum, mut top, mut tc) = (0u64, 0u64, [0u32; 10], 0usize);
        let mut cand_uninit = MaybeUninit::<[u16; 21000]>::uninit();
        let cand = cand_uninit.assume_init_mut();

        for c in 0..NUM_C {
            let (cs, cd) = (self.doc_s[c], self.doc_e[c] - self.doc_s[c]);
            if cd == 0 { continue; }
            let (rare, mut clen) = (dist[0], 0usize);
            if !self.is_bs(rare) {
                let r = self.range(c, rare);
                clen = r.len();
                if clen == 0 { continue; }
                cand[..clen].copy_from_slice(&self.g_u16[r]);
            } else {
                let cw = ((cd + 63) >> 6) as usize;
                let src = &self.g_bs[(self.bs_idx[rare] as usize) * self.w_per_doc + ((cs >> 6) as usize)..];
                for w in 0..cw {
                    let mut v = *src.get_unchecked(w);
                    while v != 0 {
                        let id = (w << 6) + v.trailing_zeros() as usize;
                        if id < cd as usize { cand[clen] = id as u16; clen += 1; }
                        v &= v - 1;
                    }
                }
                if clen == 0 { continue; }
            }

            for &ot in &dist[1..nd] {
                clen = self.intersect_and(c, ot, cs, cand, clen);
                if clen == 0 { break; }
            }

            let (d_ptr, d_len) = (self.d_buf.as_ptr(), self.d_buf.len());
            let try_match = |cand_p: usize| -> bool {
                let mut tp = cand_p;
                let mut tail = [0u8; 16];
                let (w0, w1, l) = read_tok(self.d_buf, &mut tp, &mut tail);
                if !check_match(0, l, w0, w1, &tail) { return false; }
                for j in 1..k {
                    while *CMAP.get_unchecked(*self.d_buf.get_unchecked(tp) as usize) == 0 { tp += 1; }
                    if *self.d_buf.get_unchecked(tp) == b'\n' { return false; }
                    let mut sub_tail = [0u8; 16];
                    let (sw0, sw1, sl) = read_tok(self.d_buf, &mut tp, &mut sub_tail);
                    if !check_match(j, sl, sw0, sw1, &sub_tail) { return false; }
                }
                true
            };

            for ci in 0..clen {
                let d = cs + cand[ci] as u32;
                let mut sp = *self.doc_ts.get_unchecked(d as usize) as usize;
                let mut matched = false;

                'doc_scan: loop {
                    if sp + 32 <= d_len {
                        let chunk = _mm256_loadu_si256(d_ptr.add(sp) as *const __m256i);
                        let m_ch = _mm256_or_si256(
                            _mm256_cmpeq_epi8(chunk, v_lo),
                            _mm256_cmpeq_epi8(chunk, v_up),
                        );
                        let m_nl = _mm256_cmpeq_epi8(chunk, v_nl);
                        let (mask_ch, mask_nl) = (
                            _mm256_movemask_epi8(m_ch) as u32,
                            _mm256_movemask_epi8(m_nl) as u32,
                        );
                        if mask_ch == 0 && mask_nl == 0 {
                            sp += 32;
                            continue;
                        }
                        let nl_pos = if mask_nl != 0 { mask_nl.trailing_zeros() } else { 32 };
                        let mut valid_ch = mask_ch & ((1u64 << nl_pos) - 1) as u32;

                        while valid_ch != 0 {
                            let bit = valid_ch.trailing_zeros() as usize;
                            valid_ch &= valid_ch - 1;
                            let cand_p = sp + bit;
                            if *CMAP.get_unchecked(*d_ptr.add(cand_p - 1) as usize) <= 1 && try_match(cand_p) {
                                matched = true;
                                break 'doc_scan;
                            }
                        }
                        if nl_pos < 32 {
                            break 'doc_scan;
                        }
                        sp += 32;
                    } else {
                        while *CMAP.get_unchecked(*self.d_buf.get_unchecked(sp) as usize) == 0 {
                            sp += 1;
                        }
                        if *self.d_buf.get_unchecked(sp) == b'\n' {
                            break 'doc_scan;
                        }
                        if try_match(sp) {
                            matched = true;
                            break 'doc_scan;
                        }
                        let mut tail = [0u8; 16];
                        read_tok(self.d_buf, &mut sp, &mut tail);
                    }
                }
                if matched { record_doc(d, &mut t_cnt, &mut t_sum, &mut top, &mut tc); }
            }
        }
        fmt_res(t_cnt, t_sum, &top, tc, out);
    }
}

#[target_feature(enable = "avx2,bmi1,bmi2,lzcnt,popcnt")]
unsafe fn worker_scan(
    c: usize,
    pass: u8,
    nt: usize,
    w_per_doc: usize,
    sc_ptr: *mut u32,
    doc_ts_ptr: *mut u32,
    g_bs_ptr: *mut u64,
    g_u16_ptr: *mut u16,
    bs_idx: *const u16,
    d_buf: &[u8],
    q_buf: &[u8],
    rep_map: &[u32],
    doc_s: &[u32; NUM_C],
    c_byte: &[usize; NUM_C + 1],
    seen_buf: *mut u16,
) {
    let seen = std::slice::from_raw_parts_mut(seen_buf.add(c * 81920), 81920);
    seen.fill(u16::MAX);
    let (mut p, pe, mut d, mut ld) = (c_byte[c], c_byte[c + 1], doc_s[c], 0u16);
    let co_ptr = sc_ptr.add(c * nt);
    while p < pe {
        while *d_buf.get_unchecked(p) != b'\t' { p += 1; }
        p += 1;
        if pass == 1 { *doc_ts_ptr.add(d as usize) = p as u32; }
        scan_doc(d_buf, &mut p, q_buf, rep_map, |t| {
            if *seen.get_unchecked(t) != ld {
                *seen.get_unchecked_mut(t) = ld;
                if pass == 1 {
                    *co_ptr.add(t) += 1;
                } else {
                    let bi = *bs_idx.add(t);
                    if bi != u16::MAX {
                        *g_bs_ptr.add((bi as usize) * w_per_doc + ((d as usize) >> 6)) |= 1u64 << (d & 63);
                    } else {
                        let cur = *co_ptr.add(t);
                        *g_u16_ptr.add(cur as usize) = ld;
                        *co_ptr.add(t) = cur + 1;
                    }
                }
            }
        });
        d += 1;
        ld += 1;
    }
}

#[target_feature(enable = "avx2,bmi1,bmi2,lzcnt,popcnt")]
unsafe fn run_all() {
    mallopt(-8, 1);
    mallopt(-1, 0);
    mallopt(-3, 65536);

    let mut args = std::env::args().skip(1);
    let (mut d_dir, mut o_dir) = ("/data".into(), "/out".into());
    while let Some(a) = args.next() {
        if a == "--data" { d_dir = args.next().unwrap(); }
        else if a == "--out" { o_dir = args.next().unwrap(); }
    }

     let q_buf = map_file(&format!("{}/queries.txt", d_dir));
    let d_buf = map_file(&format!("{}/docs.txt", d_dir));

    let mut queries = Vec::with_capacity(50000);
    let mut q_terms = Vec::with_capacity(160000);
    let (mut qp, qe) = (0usize, q_buf.len());
    while qp < qe {
        while qp < qe && q_buf[qp] <= b' ' { qp += 1; }
        if qp >= qe { break; }
        let ib = q_buf[qp..].starts_with(b"BOOL ");
        qp += if ib { 5 } else { 8 };
        let mut le = qp;
        while le < qe && q_buf[le] != if ib { b'\n' } else { b'"' } { le += 1; }
        let t_idx = q_terms.len() as u32;
        let mut q = Query { ib: (!ib) as u8, k: 0, ops: 0, t_idx };
        for tok in q_buf[qp..le].split(|&b| b == b' ').filter(|s| !s.is_empty()) {
            let tk = (tok.as_ptr() as usize) - (q_buf.as_ptr() as usize);
            match (ib, tok) {
                (true, b"AND") => q.ops |= 1 << ((q.k - 1) * 2),
                (true, b"OR") => q.ops |= 2 << ((q.k - 1) * 2),
                (true, b"NOT") => q.ops |= 3 << ((q.k - 1) * 2),
                _ => {
                    let tid = insert_term(tok, tk as u32, q_buf);
                    q_terms.push(tid);
                    q.k += 1;
                }
            }
        }
        queries.push(q);
        qp = le;
        while qp < qe && q_buf[qp] != b'\n' { qp += 1; }
        qp += 1;
    }
    queries.shrink_to_fit(); q_terms.shrink_to_fit();  
    let mut rep_map = vec![u32::MAX; N_TERMS];
    for q in &queries {
        if q.ib == 1 {
            let q_ts = &q_terms[q.t_idx as usize..q.t_idx as usize + q.k as usize];
            for w in q_ts.windows(2) {
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
    while pl > 0 && d_buf[pl - 1] != b'\n' { pl -= 1; }
    let mut total_docs = 0u32;
    while pl < f_sz && d_buf[pl] >= b'0' && d_buf[pl] <= b'9' {
        total_docs = total_docs * 10 + (d_buf[pl] - b'0') as u32;
        pl += 1;
    }
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
        let (mut lo, mut hi) = (0, f_sz);
        while lo < hi {
            let mut p = (lo + hi) / 2;
            while p > 0 && d_buf[p - 1] != b'\n' { p += 1; }
            let (mut id, mut cp) = (0u32, p);
            while cp < f_sz && d_buf[cp] >= b'0' && d_buf[cp] <= b'9' { id = id * 10 + (d_buf[cp] - b'0') as u32; cp += 1; }
            if p >= f_sz || id >= doc_s[c] { c_byte[c] = p; hi = (lo + hi) / 2; } else { lo = p + 1; }
        }
    }

    let nt = N_TERMS;
    let sc_count = NUM_C * nt;
    let sc_ptr = alloc_mmap::<u32>(sc_count);
    let sc_ptr_val = sc_ptr as usize;
    let doc_ts_ptr = doc_ts.as_mut_ptr() as usize;

    let w_per_doc = ((total_docs + 63) >> 6) as usize;
    let mut c_base = [0u32; NUM_C];
    let mut bs_idx = vec![u16::MAX; nt];

    let g_bs_ptr = AtomicUsize::new(0);
    let g_u16_ptr = AtomicUsize::new(0);
    let g_bs_len = AtomicUsize::new(0);
    let g_u16_len = AtomicUsize::new(0);
    let bs_idx_ptr = bs_idx.as_mut_ptr() as usize;
    let c_base_ptr = c_base.as_mut_ptr() as usize;

    let num_batches = (queries.len() + 63) / 64;
    let mut out_bufs = Vec::with_capacity(num_batches);
    for _ in 0..num_batches { out_bufs.push(Vec::<u8>::with_capacity(64 * 32)); }
    let out_bufs_ptr = out_bufs.as_mut_ptr() as usize;
    let q_counter = AtomicUsize::new(0);

    let seen_buf_ptr = alloc_mmap::<u16>(NUM_C * 81920);
    let seen_buf_val = seen_buf_ptr as usize;

    let phase = AtomicUsize::new(0);
    let ready = AtomicUsize::new(0);

     std::thread::scope(|s| {
        for tid in 1..NUM_C {
            let phase_ref = &phase;
            let ready_ref = &ready;
            let r_ref = &rep_map;
            let ds_ref = &doc_s;
            let de_ref = &doc_e;
            let cb_ref = &c_byte;
            let queries_ref = &queries;
            let q_terms_ref = &q_terms;
            let g_bs_p = &g_bs_ptr;
            let g_u16_p = &g_u16_ptr;
            let g_bs_l = &g_bs_len;
            let g_u16_l = &g_u16_len; let q_counter_ref = &q_counter;

            std::thread::Builder::new()
                .stack_size(196608)
                .spawn_scoped(s, move || {
                    worker_scan(tid, 1, nt, 0, sc_ptr_val as *mut u32, doc_ts_ptr as *mut u32, ptr::null_mut(), ptr::null_mut(), ptr::null(), d_buf, q_buf, r_ref, ds_ref, cb_ref, seen_buf_val as *mut u16);
                    ready_ref.fetch_add(1, Ordering::Release);
                    while phase_ref.load(Ordering::Acquire) < 1 { std::hint::spin_loop(); }

                    let bi_slice = std::slice::from_raw_parts(bs_idx_ptr as *const u16, nt);
                    worker_scan(tid, 2, nt, w_per_doc, sc_ptr_val as *mut u32, doc_ts_ptr as *mut u32, g_bs_p.load(Ordering::Relaxed) as *mut u64, g_u16_p.load(Ordering::Relaxed) as *mut u16, bi_slice.as_ptr(), d_buf, q_buf, r_ref, ds_ref, cb_ref, seen_buf_val as *mut u16);
                    ready_ref.fetch_add(1, Ordering::Release);
                    while phase_ref.load(Ordering::Acquire) < 2 { std::hint::spin_loop(); }

                    let c_off_slice = std::slice::from_raw_parts(sc_ptr_val as *const u32, NUM_C * nt);
                    let g_bs_slice = std::slice::from_raw_parts(g_bs_p.load(Ordering::Relaxed) as *const u64, g_bs_l.load(Ordering::Relaxed));
                    let g_u16_slice = std::slice::from_raw_parts(g_u16_p.load(Ordering::Relaxed) as *const u16, g_u16_l.load(Ordering::Relaxed));
                    let doc_ts_slice = std::slice::from_raw_parts(doc_ts_ptr as *const u32, total_docs as usize);
                    let c_base_ref = &*(c_base_ptr as *const [u32; NUM_C]);

                    let eng = Engine { c_off: c_off_slice, c_base: c_base_ref, bs_idx: bi_slice, doc_s: ds_ref, doc_e: de_ref, w_per_doc, g_bs: g_bs_slice, g_u16: g_u16_slice, rep_map: r_ref, doc_ts: doc_ts_slice, d_buf, q_buf, q_terms: q_terms_ref, nt };

                    loop {
                        let b = q_counter_ref.fetch_add(1, Ordering::Relaxed);
                        if b >= num_batches { break; }
                        let q_start = b * 64;
                        let q_end = ((b + 1) * 64).min(queries_ref.len());
                        let ob = &mut *(out_bufs_ptr as *mut Vec<u8>).add(b);
                        for q in &queries_ref[q_start..q_end] {
                            if q.ib == 0 { eng.exec_bool(q, ob); } else { eng.exec_phrase(q, ob); }
                        }
                    }
                })
                .unwrap();
        }

          worker_scan(0, 1, nt, 0, sc_ptr_val as *mut u32, doc_ts_ptr as *mut u32, ptr::null_mut(), ptr::null_mut(), ptr::null(), d_buf, q_buf, &rep_map, &doc_s, &c_byte, seen_buf_val as *mut u16);
        while ready.load(Ordering::Acquire) < NUM_C - 1 { std::hint::spin_loop(); }  
        let sc_slice = std::slice::from_raw_parts_mut(sc_ptr, sc_count);
        let mut n_bs = 0usize;
        for i in 0..nt {
            let mut df = 0u32;
            for c in 0..NUM_C { df += sc_slice[c * nt + i]; }
            TINFO[i].3 = df.min(65535) as u16;
            let th = if total_docs >= 25000 { 5000 } else { 15 }; if df > th { bs_idx[i] = n_bs as u16; n_bs += 1; }
        }

        let mut tot_u16 = 0u32;
        for c in 0..NUM_C {
            c_base[c] = tot_u16;
            let c_base_off = c * nt;
            for i in 0..nt {
                let cnt = sc_slice[c_base_off + i];
                sc_slice[c_base_off + i] = tot_u16;
                if bs_idx[i] == u16::MAX { tot_u16 += cnt; }
            }
        }

        let g_bs_total = n_bs * w_per_doc;
        let g_u16_total = tot_u16 as usize;
        let raw_bs_ptr = alloc_mmap::<u64>(g_bs_total);
        let raw_u16_ptr = alloc_mmap::<u16>(g_u16_total);

        g_bs_ptr.store(raw_bs_ptr as usize, Ordering::Relaxed);
        g_u16_ptr.store(raw_u16_ptr as usize, Ordering::Relaxed);
        g_bs_len.store(g_bs_total, Ordering::Relaxed);
        g_u16_len.store(g_u16_total, Ordering::Relaxed);

        ready.store(0, Ordering::Release);
        phase.store(1, Ordering::Release);

          worker_scan(0, 2, nt, w_per_doc, sc_ptr_val as *mut u32, doc_ts_ptr as *mut u32, raw_bs_ptr, raw_u16_ptr, bs_idx.as_ptr(), d_buf, q_buf, &rep_map, &doc_s, &c_byte, seen_buf_val as *mut u16);
        while ready.load(Ordering::Acquire) < NUM_C - 1 { std::hint::spin_loop(); }  
        free_mmap(seen_buf_ptr, NUM_C * 81920);
        madvise(VTAB.as_mut_ptr() as *mut u8, size_of::<[Slot; H_SZ]>(), 4);

        phase.store(2, Ordering::Release);

        let c_off_slice = std::slice::from_raw_parts(sc_ptr_val as *const u32, NUM_C * nt);
        let g_bs_slice = std::slice::from_raw_parts(raw_bs_ptr as *const u64, g_bs_total);
        let g_u16_slice = std::slice::from_raw_parts(raw_u16_ptr as *const u16, g_u16_total);

        let eng = Engine { c_off: c_off_slice, c_base: &c_base, bs_idx: &bs_idx, doc_s: &doc_s, doc_e: &doc_e, w_per_doc, g_bs: g_bs_slice, g_u16: g_u16_slice, rep_map: &rep_map, doc_ts: &doc_ts, d_buf, q_buf, q_terms: &q_terms, nt };
        loop {
            let b = q_counter.fetch_add(1, Ordering::Relaxed);
            if b >= num_batches { break; }
            let q_start = b * 64;
            let q_end = ((b + 1) * 64).min(queries.len());
            let ob = &mut *(out_bufs_ptr as *mut Vec<u8>).add(b);
            for q in &queries[q_start..q_end] {
                if q.ib == 0 { eng.exec_bool(q, ob); } else { eng.exec_phrase(q, ob); }
            }
        }
    });

    let _ = std::fs::create_dir_all(&o_dir);
    let mut out_file = File::create(format!("{}/results.txt", o_dir)).unwrap();
    for ob in &out_bufs { out_file.write_all(ob).unwrap(); }
    std::process::exit(0);
}

fn main() { unsafe { run_all(); } }
