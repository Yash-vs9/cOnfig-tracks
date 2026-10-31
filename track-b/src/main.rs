#![allow(static_mut_refs)]
use std::{arch::x86_64::*, fs::File, io::Write, mem::MaybeUninit, os::unix::io::IntoRawFd, ptr, sync::atomic::{AtomicUsize, Ordering}};

extern "C" {
    fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
    fn close(fd: i32) -> i32;
    fn mallopt(p: i32, v: i32) -> i32;
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
    fn malloc_trim(pad: usize) -> i32;
}

const H_SZ: usize = 131072;
const H_MSK: usize = H_SZ - 1;
const BLOOM_SZ: usize = 16384;
const BLOOM_MSK: usize = BLOOM_SZ - 1;
const NUM_C: usize = 12;
const BS_TH: u32 = 15625;
const MAX_W: usize = 352;

#[derive(Clone, Copy, Default)]
struct Slot(u32, u32);
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

#[repr(align(4096))]
struct AlignedVTab([Slot; H_SZ]);
#[repr(align(4096))]
struct AlignedBloom([u64; BLOOM_SZ]);

static mut VTAB: AlignedVTab = AlignedVTab([Slot(0, 0); H_SZ]);
static mut TINFO: [TInfo; 81920] = [TInfo(0, 0, 0, 0); 81920];
static mut BLOOM: AlignedBloom = AlignedBloom([0; BLOOM_SZ]);
static mut N_TERMS: usize = 0;

#[inline(always)]
fn mix64(w0: u64, w1: u64, len: usize) -> u64 {
    let h = (w0 ^ (w1.wrapping_mul(0x9e3779b97f4a7c15).wrapping_add(len as u64))).wrapping_mul(0xbf58476d1ce4e5b9);
    h ^ (h >> 31)
}

#[target_feature(enable = "avx2,bmi1,bmi2,lzcnt,popcnt")]
#[inline]
unsafe fn lookup_swar(w0: u64, w1: u64, tail: &[u8], len: usize, q_buf: &[u8], h: u64) -> u32 {
    let (h32, mut idx) = (((h ^ (h >> 32)) as u32) | 1, (h as usize) & H_MSK);
    loop {
        let s = *VTAB.0.get_unchecked(idx);
        if s.0 == 0 { return u32::MAX; }
        if s.0 == h32 {
            let ti = *TINFO.get_unchecked(s.1 as usize);
            if ti.0 == w0 && ti.2 as usize == len {
                if len <= 8 { return s.1; }
                let (q_ptr, off) = (q_buf.as_ptr().add(ti.1 as usize + 8), len - 8);
                let w1_b = w1.to_ne_bytes();
                let match_w1 = if off <= 8 {
                    std::slice::from_raw_parts(q_ptr, off) == &w1_b[..off] } else {
                    std::slice::from_raw_parts(q_ptr, 8) == &w1_b[..8] && std::slice::from_raw_parts(q_ptr.add(8), off - 8) == &tail[..off - 8]
                };
                if match_w1 { return s.1; }
            }
        }
        idx = (idx + 1) & H_MSK;
    }
}

#[target_feature(enable = "avx2,bmi1,bmi2,lzcnt,popcnt")]
unsafe fn insert_term(s: &[u8], off: u32, q_buf: &[u8]) -> u32 {
    let (mut w0, mut w1, len) = (0u64, 0u64, s.len());
    ptr::copy_nonoverlapping(s.as_ptr(), &mut w0 as *mut _ as *mut u8, len.min(8));
    if len > 8 { ptr::copy_nonoverlapping(s.as_ptr().add(8), &mut w1 as *mut _ as *mut u8, (len - 8).min(8)); }
    let h = mix64(w0, w1, len);
    let tail = if len > 16 { &s[16..] } else { &[] };
    let tid = lookup_swar(w0, w1, tail, len, q_buf, h);
    if tid != u32::MAX { return tid; }
    let (h32, mut idx) = (((h ^ (h >> 32)) as u32) | 1, (h as usize) & H_MSK);
    while VTAB.0[idx].0 != 0 { idx = (idx + 1) & H_MSK; }
    let nt = N_TERMS as u32;
    N_TERMS += 1;
    VTAB.0[idx] = Slot(h32, nt);
    TINFO[nt as usize] = TInfo(w0, off, len as u16, 0);
    let b_idx = ((h >> 32) as usize) & BLOOM_MSK;
    BLOOM.0[b_idx] |= (1u64 << (h & 63)) | (1u64 << ((h >> 17) & 63));
    nt
}

#[target_feature(enable = "avx2,bmi1,bmi2,lzcnt,popcnt")]
#[inline]
unsafe fn read_tok(d: &[u8], p: &mut usize, tail: &mut [u8; 16]) -> (u64, u64, usize) {
    let mut w0 = 0u64;
    let mut len = 0usize;
    let mut m = *CMAP.get_unchecked(*d.get_unchecked(*p) as usize);
    while len < 8 {
        w0 |= (m as u64) << (len * 8);
        len += 1; *p += 1;
        m = *CMAP.get_unchecked(*d.get_unchecked(*p) as usize);
        if m <= 1 { return (w0, 0, len); }
    }
    let mut w1 = 0u64;
    while len < 16 {
        w1 |= (m as u64) << ((len - 8) * 8);
        len += 1; *p += 1;
        m = *CMAP.get_unchecked(*d.get_unchecked(*p) as usize);
        if m <= 1 { return (w0, w1, len); }
    }
    while len < 32 {
        *tail.get_unchecked_mut(len - 16) = m;
        len += 1; *p += 1;
        m = *CMAP.get_unchecked(*d.get_unchecked(*p) as usize);
        if m <= 1 { return (w0, w1, len); }
    }
    while m > 1 {
        len += 1; *p += 1;
        m = *CMAP.get_unchecked(*d.get_unchecked(*p) as usize);
    }
    (w0, w1, len)
}

#[target_feature(enable = "avx2,bmi1,bmi2,lzcnt,popcnt")]
#[inline]
unsafe fn scan_doc<F: FnMut(usize)>(d_buf: &[u8], p: &mut usize, q_buf: &[u8], rep_map: &[u32], mut fn_: F) {
    let (mut pos, mut p_tid, mut p_pos) = (0u32, u32::MAX, u32::MAX);
    let d_ptr = d_buf.as_ptr();
    let d_len = d_buf.len();
    let v_20 = _mm256_set1_epi8(0x20);
    let v_a = _mm256_set1_epi8((b'a' - 1) as i8);
    let v_z = _mm256_set1_epi8((b'z' + 1) as i8);
    let v_0 = _mm256_set1_epi8((b'0' - 1) as i8);
    let v_9 = _mm256_set1_epi8((b'9' + 1) as i8);
    let v_nl = _mm256_set1_epi8(b'\n' as i8);
    let mut lbuf = [0u8; 48];

    'chunk_loop: while *p + 32 <= d_len {
        let raw = _mm256_loadu_si256(d_ptr.add(*p) as *const __m256i);
        let nl_mask = _mm256_movemask_epi8(_mm256_cmpeq_epi8(raw, v_nl)) as u32;
        let lo = _mm256_or_si256(raw, v_20);
        let is_alpha = _mm256_and_si256(_mm256_cmpgt_epi8(lo, v_a), _mm256_cmpgt_epi8(v_z, lo));
        let is_num = _mm256_and_si256(_mm256_cmpgt_epi8(raw, v_0), _mm256_cmpgt_epi8(v_9, raw));
        let mut mask = _mm256_movemask_epi8(_mm256_or_si256(is_alpha, is_num)) as u32;
        let has_nl = nl_mask != 0;
        let limit = if has_nl { nl_mask.trailing_zeros() } else { 32 };
        if has_nl { mask &= ((1u64 << limit) - 1) as u32; }
        _mm256_storeu_si256(lbuf.as_mut_ptr() as *mut __m256i, lo);

        while mask != 0 {
            let s_bit = mask.trailing_zeros();
            let shifted = mask >> s_bit;
            let t_len = shifted.trailing_ones();

            if s_bit + t_len == 32 && !has_nl {
                *p += s_bit as usize;
                let mut tail = [0u8; 16];
                let (w0, w1, len) = read_tok(d_buf, p, &mut tail);
                let h = mix64(w0, w1, len);
                let b = *BLOOM.0.get_unchecked(((h >> 32) as usize) & BLOOM_MSK);
                let m_bits = (1u64 << (h & 63)) | (1u64 << ((h >> 17) & 63));
                let tid = if (b & m_bits) == m_bits { lookup_swar(w0, w1, &tail, len, q_buf, h) } else { u32::MAX };
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
                continue 'chunk_loop;
            }

            let len = t_len as usize;
            let lp = lbuf.as_ptr().add(s_bit as usize);
            let (w0, w1, tail) = if len <= 8 {
                let raw_w0 = (lp as *const u64).read_unaligned();
                (_bzhi_u64(raw_w0, (len * 8) as u32), 0u64, [0u8; 16])
            } else if len <= 16 {
                let raw_w0 = (lp as *const u64).read_unaligned();
                let raw_w1 = (lp.add(8) as *const u64).read_unaligned();
                (raw_w0, _bzhi_u64(raw_w1, ((len - 8) * 8) as u32), [0u8; 16]) } else {
                let raw_w0 = (lp as *const u64).read_unaligned();
                let raw_w1 = (lp.add(8) as *const u64).read_unaligned();
                let mut t_arr = [0u8; 16];
                ptr::copy_nonoverlapping(lp.add(16), t_arr.as_mut_ptr(), len - 16);
                (raw_w0, raw_w1, t_arr)
            };

            let h = mix64(w0, w1, len);
            let b = *BLOOM.0.get_unchecked(((h >> 32) as usize) & BLOOM_MSK);
            let m_bits = (1u64 << (h & 63)) | (1u64 << ((h >> 17) & 63));
            let tid = if (b & m_bits) == m_bits { lookup_swar(w0, w1, &tail, len, q_buf, h) } else { u32::MAX };
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

            let clear_shift = s_bit + t_len;
            if clear_shift >= 32 { break; }
            mask &= !((1u32 << clear_shift) - 1);
        }

        if has_nl { *p += (limit as usize) + 1; return; }
        *p += 32;
    }

    loop {
        while *CMAP.get_unchecked(*d_buf.get_unchecked(*p) as usize) == 0 { *p += 1; }
        if *d_buf.get_unchecked(*p) == b'\n' { *p += 1; break; }
        let mut tail = [0u8; 16];
        let (w0, w1, len) = read_tok(d_buf, p, &mut tail);
        let h = mix64(w0, w1, len);
        let b = *BLOOM.0.get_unchecked(((h >> 32) as usize) & BLOOM_MSK);
        let m_bits = (1u64 << (h & 63)) | (1u64 << ((h >> 17) & 63));
        let tid = if (b & m_bits) == m_bits { lookup_swar(w0, w1, &tail, len, q_buf, h) } else { u32::MAX };
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
        i -= 1;
        tmp[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 { break; }
    }
    buf.extend_from_slice(&tmp[i..]);
}

#[inline(always)]
fn fmt_res(cnt: u64, sum: u64, top: &[u32; 10], tc: usize, out: &mut Vec<u8>) {
    if cnt == 0 {
        out.extend_from_slice(b"0 0\n"); } else {
        push_u64(out, cnt); out.push(b' ');
        push_u64(out, sum);
        for &d in &top[..tc] { out.push(b' '); push_u64(out, d as u64); }
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

    #[target_feature(enable = "avx2,bmi1,bmi2,lzcnt,popcnt")]
    unsafe fn exec_bool(&self, q: &Query, out: &mut Vec<u8>) {
        let (mut t_cnt, mut t_sum, mut top, mut tc) = (0u64, 0u64, [0u32; 10], 0usize);
        let mut clist_uninit = MaybeUninit::<[u16; 21000]>::uninit();
        let clist = clist_uninit.assume_init_mut();
        let mut curr_uninit = MaybeUninit::<[u64; MAX_W]>::uninit();
        let curr = curr_uninit.assume_init_mut();

        let q_ts = &self.q_terms[q.t_idx as usize..q.t_idx as usize + q.k as usize];
        let t0 = q_ts[0] as usize;

        for c in 0..NUM_C {
            let (cs, cd) = (self.doc_s[c], self.doc_e[c] - self.doc_s[c]);
            if cd == 0 { continue; }
            let cw = ((cd + 63) >> 6) as usize;
            let mut clen = 0usize;
            let mut in_bs = self.is_bs(t0);

            let get_c_bs = |tid: usize, buf: &mut [u64; MAX_W]| {
                if tid == u16::MAX as usize {
                    buf[..cw].fill(0);
                } else if self.is_bs(tid) {
                    let off = (self.bs_idx[tid] as usize) * self.w_per_doc + ((cs >> 6) as usize);
                    buf[..cw].copy_from_slice(&self.g_bs[off..off + cw]); } else {
                    buf[..cw].fill(0);
                    for &ld in &self.g_u16[self.range(c, tid)] {
                        *buf.get_unchecked_mut((ld as usize) >> 6) |= 1u64 << (ld & 63);
                    }
                }
            };

            if in_bs {
                get_c_bs(t0, curr); } else {
                let r0 = self.range(c, t0);
                clen = r0.len();
                clist[..clen].copy_from_slice(&self.g_u16[r0]);
            }

            for oi in 0..(q.k as usize - 1) {
                let op = (q.ops >> (oi * 2)) & 3;
                let nt_id = q_ts[oi + 1] as usize;
                let r = self.range(c, nt_id);

                if !in_bs && op == 2 {
                    in_bs = true;
                    curr[..cw].fill(0);
                    for i in 0..clen {
                        *curr.get_unchecked_mut((clist[i] as usize) >> 6) |= 1u64 << (clist[i] & 63);
                    }
                }

                if in_bs {
                    if self.is_bs(nt_id) {
                        let b = &self.g_bs[(self.bs_idx[nt_id] as usize) * self.w_per_doc + ((cs >> 6) as usize)..];
                        let mut w = 0usize;
                        while w + 4 <= cw {
                            let cv = _mm256_loadu_si256(curr.as_ptr().add(w) as *const __m256i);
                            let bv = _mm256_loadu_si256(b.as_ptr().add(w) as *const __m256i);
                            let rv = match op {
                                1 => _mm256_and_si256(cv, bv),
                                2 => _mm256_or_si256(cv, bv),
                                _ => _mm256_andnot_si256(bv, cv),
                            };
                            _mm256_storeu_si256(curr.as_mut_ptr().add(w) as *mut __m256i, rv);
                            w += 4;
                        }
                        while w < cw {
                            let bw = *b.get_unchecked(w);
                            let cw_ref = curr.get_unchecked_mut(w);
                            match op {
                                1 => *cw_ref &= bw,
                                2 => *cw_ref |= bw,
                                _ => *cw_ref &= !bw,
                            }
                            w += 1;
                        }
                    } else if op == 2 {
                        for &ld in &self.g_u16[r] { *curr.get_unchecked_mut((ld as usize) >> 6) |= 1u64 << (ld & 63); }
                    } else if op == 3 {
                        for &ld in &self.g_u16[r] { *curr.get_unchecked_mut((ld as usize) >> 6) &= !(1u64 << (ld & 63)); }
                    } else {
                        let mut w = 0usize;
                        for &ld in &self.g_u16[r] {
                            let idx = (ld as usize) >> 6;
                            let bit = 1u64 << (ld & 63);
                            if (*curr.get_unchecked(idx) & bit) != 0 {
                                clist[w] = ld;
                                w += 1;
                            }
                        }
                        clen = w;
                        in_bs = false;
                    }
                } else if op == 1 || op == 3 {
                    if clen == 0 { continue; }
                    let mut w = 0usize;
                    if self.is_bs(nt_id) {
                        let exp = (op == 1) as u64;
                        let b = &self.g_bs[(self.bs_idx[nt_id] as usize) * self.w_per_doc..];
                        for i in 0..clen {
                            let d = (cs + clist[i] as u32) as usize;
                            if ((*b.get_unchecked(d >> 6) >> (d & 63)) & 1) == exp {
                                clist[w] = clist[i];
                                w += 1;
                            }
                        } } else {
                        let (mut i, mut j) = (0usize, r.start);
                        while i < clen && j < r.end {
                            let (cv, gv) = (clist[i], self.g_u16[j]);
                            if (op == 1 && cv == gv) || (op == 3 && cv < gv) { clist[w] = cv; w += 1; }
                            i += (cv <= gv) as usize;
                            j += (cv >= gv) as usize;
                        }
                        if op == 3 { clist.copy_within(i..clen, w); w += clen - i; }
                    }
                    clen = w;
                }
            }

            let mut add_res = |d: u32| {
                t_cnt += 1;
                t_sum += d as u64;
                if tc < 10 { top[tc] = d; tc += 1; }
            };

            if !in_bs {
                for i in 0..clen { add_res(cs + clist[i] as u32); } } else {
                if (cd & 63) != 0 { *curr.get_unchecked_mut(cw - 1) &= (1u64 << (cd & 63)) - 1; }
                let mut w = 0usize;
                while w + 4 <= cw {
                    let v4 = _mm256_loadu_si256(curr.as_ptr().add(w) as *const __m256i);
                    if _mm256_testz_si256(v4, v4) != 0 {
                        w += 4;
                        continue;
                    }
                    for i in 0..4 {
                        let mut v = *curr.get_unchecked(w + i);
                        let base = cs + (((w + i) as u32) << 6);
                        while v != 0 { add_res(base + v.trailing_zeros()); v &= v - 1; }
                    }
                    w += 4;
                }
                while w < cw {
                    let mut v = *curr.get_unchecked(w);
                    let base = cs + ((w as u32) << 6);
                    while v != 0 { add_res(base + v.trailing_zeros()); v &= v - 1; }
                    w += 1;
                }
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
        let mut pt = [TInfo::default(); 8];
        for i in 0..k { pt[i] = *TINFO.get_unchecked(q_ts[i] as usize); }

        let check_match = |j: usize, l: usize, w0: u64, w1: u64, tail: &[u8]| -> bool {
            if l != pt[j].2 as usize || w0 != pt[j].0 { return false; }
            if l <= 8 { return true; }
            let (q_ptr, off) = (self.q_buf.as_ptr().add(pt[j].1 as usize + 8), l - 8);
            let w1_b = w1.to_ne_bytes();
            if off <= 8 {
                std::slice::from_raw_parts(q_ptr, off) == &w1_b[..off] } else {
                std::slice::from_raw_parts(q_ptr, 8) == &w1_b[..8] && std::slice::from_raw_parts(q_ptr.add(8), off - 8) == &tail[..off - 8]
            }
        };

        let first_ch = (pt[0].0 & 0xFF) as u8;
        let first_up = if first_ch >= b'a' && first_ch <= b'z' { first_ch - 32 } else { first_ch };
        let v_lo = _mm256_set1_epi8(first_ch as i8);
        let v_up = _mm256_set1_epi8(first_up as i8);
        let v_nl = _mm256_set1_epi8(b'\n' as i8);

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
                cand[..clen].copy_from_slice(&self.g_u16[r]); } else {
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
                if clen == 0 { break; }
                let mut w = 0usize;
                if self.is_bs(ot) {
                    let b = &self.g_bs[(self.bs_idx[ot] as usize) * self.w_per_doc..];
                    for i in 0..clen {
                        let d = (cs + cand[i] as u32) as usize;
                        if ((*b.get_unchecked(d >> 6) >> (d & 63)) & 1) != 0 { cand[w] = cand[i]; w += 1; }
                    } } else {
                    let r = self.range(c, ot);
                    let (mut i, mut j) = (0usize, r.start);
                    while i < clen && j < r.end {
                        let (cv, gv) = (cand[i], self.g_u16[j]);
                        if cv == gv { cand[w] = cv; w += 1; }
                        i += (cv <= gv) as usize;
                        j += (cv >= gv) as usize;
                    }
                }
                clen = w;
            }

            let d_ptr = self.d_buf.as_ptr();
            let d_len = self.d_buf.len();

            for ci in 0..clen {
                let d = cs + cand[ci] as u32;
                let mut sp = *self.doc_ts.get_unchecked(d as usize) as usize;
                let mut matched = false;

                'doc_scan: loop {
                    if sp + 32 <= d_len {
                        let chunk = _mm256_loadu_si256(d_ptr.add(sp) as *const __m256i);
                        let m_ch = _mm256_or_si256(_mm256_cmpeq_epi8(chunk, v_lo), _mm256_cmpeq_epi8(chunk, v_up));
                        let m_nl = _mm256_cmpeq_epi8(chunk, v_nl);
                        let mask_ch = _mm256_movemask_epi8(m_ch) as u32;
                        let mask_nl = _mm256_movemask_epi8(m_nl) as u32;

                        if mask_ch == 0 && mask_nl == 0 { sp += 32; continue; }
                        let nl_pos = if mask_nl != 0 { mask_nl.trailing_zeros() } else { 32 };
                        let mut valid_ch = mask_ch & ((1u64 << nl_pos) - 1) as u32;

                        while valid_ch != 0 {
                            let bit = valid_ch.trailing_zeros() as usize;
                            valid_ch &= valid_ch - 1;
                            let cand_p = sp + bit;
                            if *CMAP.get_unchecked(*d_ptr.add(cand_p - 1) as usize) <= 1 {
                                let mut tp = cand_p;
                                let mut tail = [0u8; 16];
                                let (w0, w1, l) = read_tok(self.d_buf, &mut tp, &mut tail);
                                if check_match(0, l, w0, w1, &tail) {
                                    let mut m_all = true;
                                    for j in 1..k {
                                        while *CMAP.get_unchecked(*self.d_buf.get_unchecked(tp) as usize) == 0 { tp += 1; }
                                        if *self.d_buf.get_unchecked(tp) == b'\n' { m_all = false; break; }
                                        let mut sub_tail = [0u8; 16];
                                        let (sw0, sw1, sl) = read_tok(self.d_buf, &mut tp, &mut sub_tail);
                                        if !check_match(j, sl, sw0, sw1, &sub_tail) { m_all = false; break; }
                                    }
                                    if m_all { matched = true; break 'doc_scan; }
                                }
                            }
                        }

                        if nl_pos < 32 { break 'doc_scan; }
                        sp += 32; } else {
                        while *CMAP.get_unchecked(*self.d_buf.get_unchecked(sp) as usize) == 0 { sp += 1; }
                        if *self.d_buf.get_unchecked(sp) == b'\n' { break 'doc_scan; }
                        let mut tail = [0u8; 16];
                        let (w0, w1, l) = read_tok(self.d_buf, &mut sp, &mut tail);
                        if check_match(0, l, w0, w1, &tail) {
                            let (mut xp, mut j) = (sp, 1usize);
                            while j < k {
                                while *CMAP.get_unchecked(*self.d_buf.get_unchecked(xp) as usize) == 0 { xp += 1; }
                                if *self.d_buf.get_unchecked(xp) == b'\n' { break; }
                                let mut jtail = [0u8; 16];
                                let (jw0, jw1, jl) = read_tok(self.d_buf, &mut xp, &mut jtail);
                                if !check_match(j, jl, jw0, jw1, &jtail) { break; }
                                j += 1;
                            }
                            if j == k { matched = true; break 'doc_scan; }
                        }
                    }
                }

                if matched {
                    t_cnt += 1;
                    t_sum += d as u64;
                    if tc < 10 { top[tc] = d; tc += 1; }
                }
            }
        }
        fmt_res(t_cnt, t_sum, &top, tc, out);
    }
}

#[target_feature(enable = "avx2,bmi1,bmi2,lzcnt,popcnt")]
unsafe fn pass1_worker(
    c: usize, nt: usize, sc_ptr: *mut u32, doc_ts_ptr: *mut u32,
    d_buf: &[u8], q_buf: &[u8], rep_map: &[u32], doc_s: &[u32; NUM_C], c_byte: &[usize; NUM_C + 1],
) {
    let sc_c = std::slice::from_raw_parts_mut(sc_ptr.add(c * nt), nt);
    let mut seen = [u16::MAX; 80000];
    let (mut p, pe, mut d, mut ld) = (c_byte[c], c_byte[c + 1], doc_s[c], 0u16);
    while p < pe {
        while *d_buf.get_unchecked(p) != b'\t' { p += 1; }
        p += 1;
        *doc_ts_ptr.add(d as usize) = p as u32;
        scan_doc(d_buf, &mut p, q_buf, rep_map, |t| {
            if *seen.get_unchecked(t) != ld {
                *seen.get_unchecked_mut(t) = ld;
                *sc_c.get_unchecked_mut(t) += 1;
            }
        });
        d += 1; ld += 1;
    }
}

#[target_feature(enable = "avx2,bmi1,bmi2,lzcnt,popcnt")]
unsafe fn pass2_worker(
    c: usize, nt: usize, w_per_doc: usize, g_bs_ptr: *mut u64, g_u16_ptr: *mut u16, c_off_ptr: *mut u32,
    bs_idx: &[u16], d_buf: &[u8], q_buf: &[u8], rep_map: &[u32],
    doc_s: &[u32; NUM_C], doc_e: &[u32; NUM_C], doc_ts_ptr: *const u32,
) {
    let co_ptr = c_off_ptr.add(c * nt);
    let mut seen = [u16::MAX; 80000];
    let (mut d, de) = (doc_s[c], doc_e[c]);
    let mut ld = 0u16;
    while d < de {
        let mut p = *doc_ts_ptr.add(d as usize) as usize;
        scan_doc(d_buf, &mut p, q_buf, rep_map, |t| {
            if *seen.get_unchecked(t) != ld {
                *seen.get_unchecked_mut(t) = ld;
                let bi = *bs_idx.get_unchecked(t);
                if bi != u16::MAX {
                    let d_idx = (bi as usize) * w_per_doc + ((d as usize) >> 6);
                    *g_bs_ptr.add(d_idx) |= 1u64 << (d & 63); } else {
                    let cur = *co_ptr.add(t);
                    *g_u16_ptr.add(cur as usize) = ld;
                    *co_ptr.add(t) = cur + 1;
                }
            }
        });
        d += 1; ld += 1;
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
        match a.as_str() {
            "--data" => d_dir = args.next().unwrap(),
            "--out" => o_dir = args.next().unwrap(),
            _ => {}
        }
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
    queries.shrink_to_fit();
    q_terms.shrink_to_fit();

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
            while cp < f_sz && d_buf[cp] >= b'0' && d_buf[cp] <= b'9' {
                id = id * 10 + (d_buf[cp] - b'0') as u32;
                cp += 1;
            }
            if p >= f_sz || id >= doc_s[c] { c_byte[c] = p; hi = (lo + hi) / 2; } else { lo = p + 1; }
        }
    }

    let nt = N_TERMS;
    let mut sc = vec![0u32; NUM_C * nt];
    let sc_ptr = sc.as_mut_ptr() as usize;
    let doc_ts_ptr = doc_ts.as_mut_ptr() as usize;

    let w_per_doc = ((total_docs + 63) >> 6) as usize;
    let mut c_base = [0u32; NUM_C];
    let mut bs_idx = vec![u16::MAX; nt];
    let mut g_bs: Vec<u64> = Vec::new();
    let mut g_u16: Vec<u16> = Vec::new();

    let g_bs_ptr = AtomicUsize::new(0);
    let g_u16_ptr = AtomicUsize::new(0);
    let g_bs_len = AtomicUsize::new(0);
    let g_u16_len = AtomicUsize::new(0);
    let bs_idx_ptr = bs_idx.as_mut_ptr() as usize;
    let c_base_ptr = c_base.as_mut_ptr() as usize;

    const NUM_Q: usize = 12;
    let chunk_q = (queries.len() + NUM_Q - 1) / NUM_Q;
    let mut out_bufs = vec![Vec::<u8>::with_capacity(chunk_q * 30); NUM_Q];
    let out_bufs_ptr = out_bufs.as_mut_ptr() as usize;

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
            let g_u16_l = &g_u16_len;

            std::thread::Builder::new()
                .stack_size(196608)
                .spawn_scoped(s, move || {
                    pass1_worker(tid, nt, sc_ptr as *mut u32, doc_ts_ptr as *mut u32, d_buf, q_buf, r_ref, ds_ref, cb_ref);
                    ready_ref.fetch_add(1, Ordering::Release);
                    while phase_ref.load(Ordering::Acquire) < 1 { std::hint::spin_loop(); }

                    let bi_slice = std::slice::from_raw_parts(bs_idx_ptr as *const u16, nt);
                    pass2_worker(tid, nt, w_per_doc, g_bs_p.load(Ordering::Relaxed) as *mut u64, g_u16_p.load(Ordering::Relaxed) as *mut u16, sc_ptr as *mut u32, bi_slice, d_buf, q_buf, r_ref, ds_ref, de_ref, doc_ts_ptr as *const u32);
                    ready_ref.fetch_add(1, Ordering::Release);
                    while phase_ref.load(Ordering::Acquire) < 2 { std::hint::spin_loop(); }

                    let c_off_slice = std::slice::from_raw_parts(sc_ptr as *const u32, NUM_C * nt);
                    let g_bs_slice = std::slice::from_raw_parts(g_bs_p.load(Ordering::Relaxed) as *const u64, g_bs_l.load(Ordering::Relaxed));
                    let g_u16_slice = std::slice::from_raw_parts(g_u16_p.load(Ordering::Relaxed) as *const u16, g_u16_l.load(Ordering::Relaxed));
                    let doc_ts_slice = std::slice::from_raw_parts(doc_ts_ptr as *const u32, total_docs as usize);
                    let c_base_ref = &*(c_base_ptr as *const [u32; NUM_C]);

                    let eng = Engine {
                        c_off: c_off_slice, c_base: c_base_ref, bs_idx: bi_slice, doc_s: ds_ref, doc_e: de_ref,
                        w_per_doc, g_bs: g_bs_slice, g_u16: g_u16_slice, rep_map: r_ref, doc_ts: doc_ts_slice,
                        d_buf, q_buf, q_terms: q_terms_ref, nt,
                    };

                    let q_start = (tid * chunk_q).min(queries_ref.len());
                    let q_end = ((tid + 1) * chunk_q).min(queries_ref.len());
                    let ob = &mut *(out_bufs_ptr as *mut Vec<u8>).add(tid);
                    for q in &queries_ref[q_start..q_end] {
                        if q.ib == 0 { eng.exec_bool(q, ob); } else { eng.exec_phrase(q, ob); }
                    }
                })
                .unwrap();
        }

        pass1_worker(0, nt, sc_ptr as *mut u32, doc_ts_ptr as *mut u32, d_buf, q_buf, &rep_map, &doc_s, &c_byte);
        while ready.load(Ordering::Acquire) < NUM_C - 1 { std::hint::spin_loop(); }

        let mut n_bs = 0usize;
        for i in 0..nt {
            let mut df = 0u32;
            for c in 0..NUM_C { df += sc[c * nt + i]; }
            TINFO[i].3 = df.min(65535) as u16;
            if df > BS_TH { bs_idx[i] = n_bs as u16; n_bs += 1; }
        }

        let mut tot_u16 = 0u32;
        for c in 0..NUM_C {
            c_base[c] = tot_u16;
            let c_base_off = c * nt;
            for i in 0..nt {
                let cnt = sc[c_base_off + i];
                sc[c_base_off + i] = tot_u16;
                if bs_idx[i] == u16::MAX { tot_u16 += cnt; }
            }
        }

        g_bs = vec![0u64; n_bs * w_per_doc];
        g_u16 = vec![0u16; tot_u16 as usize];
        g_bs_ptr.store(g_bs.as_mut_ptr() as usize, Ordering::Relaxed);
        g_u16_ptr.store(g_u16.as_mut_ptr() as usize, Ordering::Relaxed);
        g_bs_len.store(n_bs * w_per_doc, Ordering::Relaxed);
        g_u16_len.store(tot_u16 as usize, Ordering::Relaxed);

        ready.store(0, Ordering::Release);
        phase.store(1, Ordering::Release);

        pass2_worker(0, nt, w_per_doc, g_bs.as_mut_ptr(), g_u16.as_mut_ptr(), sc.as_mut_ptr(), &bs_idx, d_buf, q_buf, &rep_map, &doc_s, &doc_e, doc_ts.as_ptr());
        while ready.load(Ordering::Acquire) < NUM_C - 1 { std::hint::spin_loop(); }

        madvise(VTAB.0.as_mut_ptr() as *mut u8, std::mem::size_of_val(&VTAB), 4);
        madvise(BLOOM.0.as_mut_ptr() as *mut u8, std::mem::size_of_val(&BLOOM), 4);

        phase.store(2, Ordering::Release);

        let eng = Engine {
            c_off: &sc, c_base: &c_base, bs_idx: &bs_idx, doc_s: &doc_s, doc_e: &doc_e,
            w_per_doc, g_bs: &g_bs, g_u16: &g_u16, rep_map: &rep_map, doc_ts: &doc_ts,
            d_buf, q_buf, q_terms: &q_terms, nt,
        };
        let q_end = chunk_q.min(queries.len());
        let ob = &mut *(out_bufs_ptr as *mut Vec<u8>);
        for q in &queries[0..q_end] {
            if q.ib == 0 { eng.exec_bool(q, ob); } else { eng.exec_phrase(q, ob); }
        }
    });

    drop(g_bs);
    drop(g_u16);
    drop(sc);
    drop(doc_ts);
    drop(queries);
    drop(q_terms);
    malloc_trim(0);

    let mut out_file = File::create(format!("{}/results.txt", o_dir)).unwrap();
    for ob in &out_bufs { out_file.write_all(ob).unwrap(); }
}

fn main() {
    unsafe { run_all(); }
}
