#![allow(static_mut_refs)]

const BIT_SUM_TABLE: [[u16; 256]; 8] = {
    let mut tbl = [[0u16; 256]; 8];
    let mut k = 0usize;
    while k < 8 {
        let mut b = 0usize;
        while b < 256 {
            let mut s = 0u16;
            let mut i = 0usize;
            while i < 8 {
                if (b & (1 << i)) != 0 {
                    s += (k * 8 + i) as u16;
                }
                i += 1;
            }
            tbl[k][b] = s;
            b += 1;
        }
        k += 1;
    }
    tbl
};

#[inline(always)]
fn word_bit_sum(val: u64) -> u64 {
    let b = val.to_le_bytes();
    let mut s = 0u64;
    for i in 0..8 {
        s += BIT_SUM_TABLE[i][b[i] as usize] as u64;
    }
    s
}

use std::{
    arch::x86_64::*,
    fs::File,
    io::Write,
    mem::MaybeUninit,
    os::unix::io::IntoRawFd,
    ptr,
    sync::atomic::{AtomicU32, AtomicUsize, Ordering},
};

extern "C" {
    fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
    fn munmap(a: *mut u8, l: usize) -> i32;
    fn mallopt(p: i32, v: i32) -> i32;
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
    fn sched_setaffinity(pid: i32, cpusetsize: usize, mask: *const u64) -> i32;
    fn _exit(status: i32) -> !;
}

const H_SZ: usize = 131072;
const H_MSK: usize = H_SZ - 1;
const NUM_T: usize = 12;
const NUM_C: usize = 24;
const FW_CAP: usize = 650_000;
const DP_CAP: usize = 12_000;
const FC_SZ: usize = 2048;
const FC_MSK: usize = FC_SZ - 1;
const PF_WORDS: usize = 16384;
const PF_WMASK: usize = PF_WORDS - 1;

#[derive(Clone, Copy, Default)]
#[repr(C, align(16))]
struct Slot {
    w0: u64,
    h_tag: u32,
    tid: u32,
}

#[derive(Clone, Copy, Default)]
struct TInfo {
    w0: u64,
    off: u32,
    df: u32,
    len: u16,
}

#[derive(Clone, Copy)]
struct Query {
    ib: u8,
    k: u8,
    ops: u16,
    t_idx: u32,
}

#[derive(Clone, Copy, Default)]
#[repr(C, align(16))]
struct FastEntry {
    w0: u64,
    tid: u32,
    bi: u16,
    len: u8,
    _pad: u8,
}

#[derive(Clone, Copy, Default)]
#[repr(C, align(32))]
struct FastBucket {
    e0: FastEntry,
    e1: FastEntry,
}

const EMPTY_FE: FastEntry = FastEntry {
    w0: 0,
    tid: u32::MAX,
    bi: u16::MAX,
    len: 0,
    _pad: 0,
};
const EMPTY_FB: FastBucket = FastBucket {
    e0: EMPTY_FE,
    e1: EMPTY_FE,
};
static mut FAST_CACHE: [FastBucket; FC_SZ] = [EMPTY_FB; FC_SZ];

const CMAP: [u8; 256] = {
    let mut m = [0u8; 256];
    m[b'\n' as usize] = 1;
    let mut i = 0usize;
    while i < 256 {
        if i >= b'0' as usize && i <= b'9' as usize {
            m[i] = i as u8;
        }
        if i >= b'a' as usize && i <= b'z' as usize {
            m[i] = i as u8;
            m[i - 32] = i as u8;
        }
        i += 1;
    }
    m
};

const EMPTY_SLOT: Slot = Slot {
    w0: 0,
    h_tag: 0,
    tid: 0,
};
static mut VTAB: [Slot; H_SZ] = [EMPTY_SLOT; H_SZ];
const EMPTY_TI: TInfo = TInfo {
    w0: 0,
    off: 0,
    df: 0,
    len: 0,
};
static mut TINFO: [TInfo; 81920] = [EMPTY_TI; 81920];
static mut GLOBAL_DF: [AtomicU32; 81920] = [const { AtomicU32::new(0) }; 81920];
static mut N_TERMS: usize = 0;
static mut PF: [u64; PF_WORDS] = [0; PF_WORDS];

#[inline(always)]
fn mix64(w0: u64, w1: u64, len: usize) -> u64 {
    let h = (w0 ^ (w1.wrapping_mul(0x9e3779b97f4a7c15).wrapping_add(len as u64)))
        .wrapping_mul(0xbf58476d1ce4e5b9);
    h ^ (h >> 31)
}

#[inline(always)]
unsafe fn lookup_swar(w0: u64, h: u64, len: usize) -> u32 {
    let (h32, mut idx) = (((h ^ (h >> 32)) as u32) | 1, (h as usize) & H_MSK);
    let h_tag = (h32 & 0x00FFFFFF) | ((len as u32) << 24);
    loop {
        let s = VTAB.get_unchecked(idx);
        if s.h_tag == 0 {
            return u32::MAX;
        }
        if s.h_tag == h_tag && s.w0 == w0 {
            return s.tid;
        }
        idx = (idx + 1) & H_MSK;
    }
}

unsafe fn insert_term(s: &[u8], off: u32) -> u32 {
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
    let (h32, mut idx) = (((h ^ (h >> 32)) as u32) | 1, (h as usize) & H_MSK);
    let h_tag = (h32 & 0x00FFFFFF) | ((len as u32) << 24);
    loop {
        let slot = VTAB.get_unchecked_mut(idx);
        if slot.h_tag == 0 {
            let nt = N_TERMS as u32;
            N_TERMS += 1;
            *slot = Slot { w0, h_tag, tid: nt };
            TINFO[nt as usize] = TInfo {
                w0,
                off,
                df: 0,
                len: len as u16,
            };
            let widx = ((h ^ (h >> 16)) as usize) & PF_WMASK;
            PF[widx] |= (1u64 << ((h >> 32) & 63)) | (1u64 << ((h >> 40) & 63));
            return nt;
        }
        if slot.h_tag == h_tag && slot.w0 == w0 {
            return slot.tid;
        }
        idx = (idx + 1) & H_MSK;
    }
}

#[inline(always)]
unsafe fn tokmask(c: __m256i) -> u32 {
    let lo = _mm256_or_si256(c, _mm256_set1_epi8(0x20));
    let t = _mm256_sub_epi8(lo, _mm256_set1_epi8(0x61));
    let l = _mm256_cmpeq_epi8(_mm256_min_epu8(t, _mm256_set1_epi8(25)), t);
    let d = _mm256_sub_epi8(c, _mm256_set1_epi8(0x30));
    let n = _mm256_cmpeq_epi8(_mm256_min_epu8(d, _mm256_set1_epi8(9)), d);
    _mm256_movemask_epi8(_mm256_or_si256(l, n)) as u32
}

#[inline(always)]
unsafe fn read_tok(d: &[u8], p: &mut usize) -> (u64, u64, usize) {
    let (mut w0, mut w1, mut len) = (0u64, 0u64, 0usize);
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
    while len < 16 {
        w1 |= (m as u64) << ((len - 8) * 8);
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

#[target_feature(enable = "avx2,bmi1,bmi2")]
unsafe fn scan_doc<F: FnMut(usize)>(
    d_buf: &[u8],
    p: &mut usize,
    g_bs_ptr: *mut u64,
    w_per_doc: usize,
    d_w: usize,
    d_b: u64,
    mut fn_: F,
) {
    let (base, n) = (d_buf.as_ptr(), d_buf.len());
    let nlv = _mm256_set1_epi8(b'\n' as i8);
    let mut process_token = |w0: u64, w1: u64, len: usize| {
        if len <= 8 {
            let fc_slot = (w0.wrapping_mul(0x9e3779b97f4a7c15) >> 32) as usize & FC_MSK;
            let b = FAST_CACHE.get_unchecked(fc_slot);
            let mut fc_ptr = ptr::null();
            if b.e0.w0 == w0 && b.e0.len == len as u8 {
                fc_ptr = &b.e0 as *const FastEntry;
            } else if b.e1.w0 == w0 && b.e1.len == len as u8 {
                fc_ptr = &b.e1 as *const FastEntry;
            }
            if !fc_ptr.is_null() {
                let fc = &*fc_ptr;
                if fc.bi != u16::MAX {
                    *g_bs_ptr.add((fc.bi as usize) * w_per_doc + d_w) |= d_b;
                } else {
                    fn_(fc.tid as usize);
                }
                return;
            }
        }
        let h = mix64(w0, w1, len);
        let widx = ((h ^ (h >> 16)) as usize) & PF_WMASK;
        let m = (1u64 << ((h >> 32) & 63)) | (1u64 << ((h >> 40) & 63));
        if (*PF.get_unchecked(widx) & m) != m {
            return;
        }
        let tid = lookup_swar(w0, h, len);
        if tid != u32::MAX {
            fn_(tid as usize);
        }
    };
    while *p + 64 <= n {
        let c0 = _mm256_loadu_si256(base.add(*p) as *const __m256i);
        let c1 = _mm256_loadu_si256(base.add(*p + 32) as *const __m256i);
        let mask0 = tokmask(c0);
        let mask1 = tokmask(c1);
        let nl0 = _mm256_cmpeq_epi8(c0, nlv);
        let nl1 = _mm256_cmpeq_epi8(c1, nlv);
        let m_nl0 = _mm256_movemask_epi8(nl0) as u32;
        let m_nl1 = _mm256_movemask_epi8(nl1) as u32;
        let mask_nl = (m_nl0 as u64) | ((m_nl1 as u64) << 32);
        let mut mask_tok = (mask0 as u64) | ((mask1 as u64) << 32);
        let nl_pos = if mask_nl != 0 {
            mask_nl.trailing_zeros() as usize
        } else {
            64
        };
        if mask_nl != 0 {
            mask_tok &= if nl_pos == 64 {
                u64::MAX
            } else {
                (1u64 << nl_pos).wrapping_sub(1)
            };
        }
        let mut offset = 0usize;
        let mut crossed = false;
        while mask_tok != 0 {
            let zeros = mask_tok.trailing_zeros() as usize;
            let tok_start = offset + zeros;
            let rem = mask_tok >> zeros;
            let len = (!rem).trailing_zeros() as usize;
            if mask_nl == 0 && tok_start + len == 64 {
                *p += tok_start;
                let r = read_tok(d_buf, p);
                process_token(r.0, r.1, r.2);
                crossed = true;
                break;
            }
            let (w0, w1) = if len <= 8 {
                let raw = ptr::read_unaligned(base.add(*p + tok_start) as *const u64)
                    | 0x2020202020202020;
                (core::arch::x86_64::_bzhi_u64(raw, (len << 3) as u32), 0)
            } else if len <= 16 {
                let r0 = ptr::read_unaligned(base.add(*p + tok_start) as *const u64)
                    | 0x2020202020202020;
                let r1 = ptr::read_unaligned(base.add(*p + tok_start + 8) as *const u64)
                    | 0x2020202020202020;
                (
                    r0,
                    core::arch::x86_64::_bzhi_u64(r1, ((len - 8) << 3) as u32),
                )
            } else {
                let mut tp = *p + tok_start;
                let r = read_tok(d_buf, &mut tp);
                (r.0, r.1)
            };
            process_token(w0, w1, len);
            let consumed = zeros + len;
            if consumed >= 64 {
                break;
            }
            mask_tok = rem >> len;
            offset = tok_start + len;
        }
        if mask_nl != 0 {
            *p += nl_pos + 1;
            return;
        }
        if !crossed {
            *p += 64;
        }
    }
    while *p < n {
        while *p < n && *CMAP.get_unchecked(*base.add(*p) as usize) == 0 {
            *p += 1;
        }
        if *p >= n || *base.add(*p) == b'\n' {
            *p += 1;
            break;
        }
        let r = read_tok(d_buf, p);
        process_token(r.0, r.1, r.2);
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
    let l = f.metadata().unwrap().len() as usize;
    let p = mmap(ptr::null_mut(), l, 1, 1, f.into_raw_fd(), 0);
    std::slice::from_raw_parts(p, l)
}

unsafe fn alloc_mmap<T>(count: usize) -> *mut T {
    let sz = (count * std::mem::size_of::<T>() + 4095) & !4095;
    mmap(ptr::null_mut(), sz, 3, 0x22, -1, 0) as *mut T
}

unsafe fn free_mmap<T>(p: *mut T, count: usize) {
    if !p.is_null() {
        let sz = (count * std::mem::size_of::<T>() + 4095) & !4095;
        munmap(p as *mut u8, sz);
    }
}

struct Engine<'a> {
    c_offs: &'a [u32],
    c_postings: [*const u16; NUM_C],
    bs_idx: &'a [u16],
    doc_s: &'a [u32; NUM_C],
    doc_e: &'a [u32; NUM_C],
    w_per_doc: usize,
    g_bs: &'a [u64],
    doc_ts: &'a [u32],
    d_buf: &'a [u8],
    q_buf: &'a [u8],
    q_terms: &'a [u32],
    nt: usize,
}

impl<'a> Engine<'a> {
    #[inline(always)]
    unsafe fn chunk_postings(&self, c: usize, tid: usize) -> &[u16] {
        let base = c * self.nt;
        let s = if tid > 0 {
            *self.c_offs.get_unchecked(base + tid - 1)
        } else {
            0
        } as usize;
        let e = *self.c_offs.get_unchecked(base + tid) as usize;
        std::slice::from_raw_parts(self.c_postings[c].add(s), e - s)
    }

    #[inline(always)]
    unsafe fn is_bs(&self, tid: usize) -> bool {
        *self.bs_idx.get_unchecked(tid) != u16::MAX
    }

    #[inline(always)]
    unsafe fn intersect_and(
        &self,
        c: usize,
        t: usize,
        cs: u32,
        cand: &mut [u16],
        clen: usize,
    ) -> usize {
        let mut w = 0usize;
        if self.is_bs(t) {
            let src =
                &self.g_bs[(self.bs_idx[t] as usize) * self.w_per_doc + ((cs >> 6) as usize)..];
            for i in 0..clen {
                let ld = cand[i] as usize;
                cand[w] = ld as u16;
                w += ((*src.get_unchecked(ld >> 6) >> (ld & 63)) & 1) as usize;
            }
        } else {
            let o_post = self.chunk_postings(c, t);
            let olen = o_post.len();
            if olen == 0 {
                return 0;
            }
            if olen > clen * 8 {
                let mut j = 0usize;
                for i in 0..clen {
                    let v = cand[i];
                    if j < olen {
                        let sub = &o_post[j..];
                        let idx = sub.partition_point(|&x| x < v);
                        j += idx;
                        if j < olen && o_post[j] == v {
                            cand[w] = v;
                            w += 1;
                            j += 1;
                        }
                    }
                }
            } else {
                let (mut i, mut j) = (0usize, 0usize);
                while i < clen && j < olen {
                    let (v1, v2) = (cand[i], *o_post.get_unchecked(j));
                    if v1 == v2 {
                        cand[w] = v1;
                        w += 1;
                        i += 1;
                        j += 1;
                    } else if v1 < v2 {
                        i += 1;
                    } else {
                        j += 1;
                    }
                }
            }
        }
        w
    }

    #[inline(always)]
    unsafe fn intersect_not(
        &self,
        c: usize,
        t: usize,
        cs: u32,
        cand: &mut [u16],
        clen: usize,
    ) -> usize {
        let mut w = 0usize;
        if self.is_bs(t) {
            let src =
                &self.g_bs[(self.bs_idx[t] as usize) * self.w_per_doc + ((cs >> 6) as usize)..];
            for i in 0..clen {
                let ld = cand[i] as usize;
                cand[w] = ld as u16;
                w += (((*src.get_unchecked(ld >> 6) >> (ld & 63)) & 1) ^ 1) as usize;
            }
        } else {
            let o_post = self.chunk_postings(c, t);
            let (mut i, mut j, olen) = (0usize, 0usize, o_post.len());
            while i < clen && j < olen {
                let (v1, v2) = (cand[i], *o_post.get_unchecked(j));
                if v1 == v2 {
                    i += 1;
                    j += 1;
                } else if v1 < v2 {
                    cand[w] = v1;
                    w += 1;
                    i += 1;
                } else {
                    j += 1;
                }
            }
            while i < clen {
                cand[w] = cand[i];
                w += 1;
                i += 1;
            }
        }
        w
    }

    #[target_feature(enable = "avx2,bmi1,bmi2,lzcnt,popcnt")]
    unsafe fn emit_bits(
        &self,
        curr: &[u64],
        cs: u32,
        cd: u32,
        cw: usize,
        t_cnt: &mut u64,
        t_sum: &mut u64,
        top: &mut [u32; 10],
        tc: &mut usize,
    ) {
        let mut w = 0usize;
        while w < cw {
            if w + 4 <= cw {
                let v = _mm256_loadu_si256(curr.as_ptr().add(w) as *const __m256i);
                if _mm256_testz_si256(v, v) != 0 {
                    w += 4;
                    continue;
                }
            }
            let mut val = *curr.get_unchecked(w);
            if w == (cd >> 6) as usize && (cd & 63) != 0 {
                val &= (1u64 << (cd & 63)) - 1;
            }
            if val != 0 {
                let base_id = cs + (w << 6) as u32;
                if *tc < 10 {
                    while val != 0 {
                        let bit = val.trailing_zeros();
                        let d = base_id + bit;
                        *t_cnt += 1;
                        *t_sum += d as u64;
                        if *tc < 10 {
                            top[*tc] = d;
                            *tc += 1;
                        }
                        val &= val - 1;
                    }
                } else {
                    let cnt = val.count_ones() as u64;
                    *t_cnt += cnt;
                    *t_sum += cnt * (base_id as u64) + word_bit_sum(val);
                }
            }
            w += 1;
        }
    }

    #[target_feature(enable = "avx2,bmi1,bmi2,lzcnt,popcnt")]
    unsafe fn exec_bool(&self, q: &Query, out: &mut Vec<u8>) {
        let (mut t_cnt, mut t_sum, mut top, mut tc) = (0u64, 0u64, [0u32; 10], 0usize);
        let q_ts = &self.q_terms[q.t_idx as usize..q.t_idx as usize + q.k as usize];
        let mut curr_uninit = MaybeUninit::<[u64; 352]>::uninit();
        let curr = curr_uninit.assume_init_mut();
        let mut clist_uninit = MaybeUninit::<[u16; 21000]>::uninit();
        let clist = clist_uninit.assume_init_mut();

        let is_pure_and = (1..q.k as usize).all(|i| ((q.ops >> ((i - 1) * 2)) & 3) == 1);
        let mut sorted_ts = [0u32; 16];
        let q_ts = if is_pure_and {
            for &tid in q_ts {
                if TINFO[tid as usize].df == 0 {
                    return fmt_res(0, 0, &[0; 10], 0, out);
                }
            }
            sorted_ts[..q.k as usize].copy_from_slice(q_ts);
            sorted_ts[..q.k as usize]
                .sort_unstable_by_key(|&tid| (self.is_bs(tid as usize), TINFO[tid as usize].df));
            &sorted_ts[..q.k as usize]
        } else {
            q_ts
        };
        for c in 0..NUM_C {
            let (cs, cd) = (self.doc_s[c], self.doc_e[c] - self.doc_s[c]);
            if cd == 0 {
                continue;
            }
            let cw = (cd as usize + 63) >> 6;
            let (mut in_bs, mut clen) = (false, 0usize);
            let t0 = q_ts[0] as usize;
            if self.is_bs(t0) {
                in_bs = true;
                let src = &self.g_bs
                    [(self.bs_idx[t0] as usize) * self.w_per_doc + ((cs >> 6) as usize)..];
                curr[..cw].copy_from_slice(&src[..cw]);
            } else {
                let p = self.chunk_postings(c, t0);
                clen = p.len();
                clist[..clen].copy_from_slice(p);
            }
            for i in 0..(q.k - 1) as usize {
                let op = ((q.ops >> (i * 2)) & 3) as u8;
                let nt_id = q_ts[i + 1] as usize;
                if !in_bs && op == 2 {
                    curr[..cw].fill(0);
                    for &ld in &clist[..clen] {
                        *curr.get_unchecked_mut((ld as usize) >> 6) |= 1u64 << (ld & 63);
                    }
                    in_bs = true;
                }
                if in_bs {
                    if self.is_bs(nt_id) {
                        let src = &self.g_bs[(self.bs_idx[nt_id] as usize) * self.w_per_doc
                            + ((cs >> 6) as usize)..];
                        let mut w = 0usize;
                        while w + 4 <= cw {
                            let (c_v, s_v) = (
                                _mm256_loadu_si256(curr.as_ptr().add(w) as *const __m256i),
                                _mm256_loadu_si256(src.as_ptr().add(w) as *const __m256i),
                            );
                            let res = match op {
                                1 => _mm256_and_si256(c_v, s_v),
                                2 => _mm256_or_si256(c_v, s_v),
                                _ => _mm256_andnot_si256(s_v, c_v),
                            };
                            _mm256_storeu_si256(curr.as_mut_ptr().add(w) as *mut __m256i, res);
                            w += 4;
                        }
                        while w < cw {
                            let s = *src.get_unchecked(w);
                            match op {
                                1 => *curr.get_unchecked_mut(w) &= s,
                                2 => *curr.get_unchecked_mut(w) |= s,
                                _ => *curr.get_unchecked_mut(w) &= !s,
                            }
                            w += 1;
                        }
                    } else if op == 1 {
                        let mut w = 0usize;
                        for &ld in self.chunk_postings(c, nt_id) {
                            if (*curr.get_unchecked((ld as usize) >> 6) & (1u64 << (ld & 63))) != 0
                            {
                                clist[w] = ld;
                                w += 1;
                            }
                        }
                        clen = w;
                        in_bs = false;
                    } else if op == 2 {
                        for &ld in self.chunk_postings(c, nt_id) {
                            *curr.get_unchecked_mut((ld as usize) >> 6) |= 1u64 << (ld & 63);
                        }
                    } else {
                        for &ld in self.chunk_postings(c, nt_id) {
                            *curr.get_unchecked_mut((ld as usize) >> 6) &= !(1u64 << (ld & 63));
                        }
                    }
                } else if op == 1 {
                    clen = self.intersect_and(c, nt_id, cs, clist, clen);
                } else if op == 3 {
                    clen = self.intersect_not(c, nt_id, cs, clist, clen);
                }
            }
            if !in_bs {
                t_cnt += clen as u64;
                let need = (10 - tc).min(clen);
                for i in 0..need {
                    top[tc + i] = cs + clist[i] as u32;
                }
                tc += need;
                let mut s = 0u64;
                for &ld in &clist[..clen] {
                    s += ld as u64;
                }
                t_sum += (cs as u64) * (clen as u64) + s;
            } else {
                self.emit_bits(curr, cs, cd, cw, &mut t_cnt, &mut t_sum, &mut top, &mut tc);
            }
        }
        fmt_res(t_cnt, t_sum, &top, tc, out);
    }

    #[target_feature(enable = "avx2,bmi1,bmi2,lzcnt,popcnt")]
    unsafe fn exec_phrase(&self, q: &Query, out: &mut Vec<u8>) {
        let (k, mut nd, mut dist) = (q.k as usize, 0usize, [0usize; 16]);
        let q_ts = &self.q_terms[q.t_idx as usize..q.t_idx as usize + k];
        let mut add_dist = |t: usize| {
            if t == u32::MAX as usize || TINFO[t].df == 0 {
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
            let t = q_ts[i] as usize;
            if !add_dist(t) {
                return fmt_res(0, 0, &[0; 10], 0, out);
            }
        }
        dist[..nd].sort_unstable_by_key(|&t| TINFO[t].df);
        let (mut t_cnt, mut t_sum, mut top, mut tc) = (0u64, 0u64, [0u32; 10], 0usize);
        let mut cand_uninit = MaybeUninit::<[u16; 21000]>::uninit();
        let cand = cand_uninit.assume_init_mut();
        let record_doc =
            |d: u32, t_cnt: &mut u64, t_sum: &mut u64, top: &mut [u32; 10], tc: &mut usize| {
                *t_cnt += 1;
                *t_sum += d as u64;
                if *tc < 10 {
                    top[*tc] = d;
                    *tc += 1;
                }
            };
        for c in 0..NUM_C {
            let (cs, cd) = (self.doc_s[c], self.doc_e[c] - self.doc_s[c]);
            if cd == 0 {
                continue;
            }
            let (rare, mut clen) = (dist[0], 0usize);
            if !self.is_bs(rare) {
                let p = self.chunk_postings(c, rare);
                clen = p.len();
                if clen == 0 {
                    continue;
                }
                cand[..clen].copy_from_slice(p);
            } else {
                let cw = (cd as usize + 63) >> 6;
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
                clen = self.intersect_and(c, ot, cs, cand, clen);
                if clen == 0 {
                    break;
                }
            }
            if k == 1 {
                for ci in 0..clen {
                    let d = cs + cand[ci] as u32;
                    record_doc(d, &mut t_cnt, &mut t_sum, &mut top, &mut tc);
                }
                continue;
            }
            let mut phrase_words: [&[u8]; 8] = [&[]; 8];
            for i in 0..k {
                let ti = *TINFO.get_unchecked(q_ts[i] as usize);
                phrase_words[i] = &self.q_buf[ti.off as usize..ti.off as usize + ti.len as usize];
            }
            #[inline(always)]
            fn word_eq(src: &[u8], target: &[u8]) -> bool {
                if src.len() != target.len() {
                    return false;
                }
                for i in 0..src.len() {
                    if (src[i] | 0x20) != target[i] {
                        return false;
                    }
                }
                true
            }
            for ci in 0..clen {
                let d = cs + cand[ci] as u32;
                let mut p = *self.doc_ts.get_unchecked(d as usize) as usize;
                let n = self.d_buf.len();
                let mut matched = 0usize;
                while p < n && *self.d_buf.get_unchecked(p) != b'\n' {
                    while p < n {
                        let b = *self.d_buf.get_unchecked(p);
                        if b == b'\n'
                            || (b >= b'0' && b <= b'9')
                            || (b | 0x20 >= b'a' && b | 0x20 <= b'z')
                        {
                            break;
                        }
                        p += 1;
                    }
                    if p >= n || *self.d_buf.get_unchecked(p) == b'\n' {
                        break;
                    }
                    let start = p;
                    while p < n {
                        let b = *self.d_buf.get_unchecked(p);
                        if !((b >= b'0' && b <= b'9') || (b | 0x20 >= b'a' && b | 0x20 <= b'z')) {
                            break;
                        }
                        p += 1;
                    }
                    let w = &self.d_buf[start..p];
                    if word_eq(w, phrase_words[matched]) {
                        matched += 1;
                        if matched == k {
                            break;
                        }
                    } else if word_eq(w, phrase_words[0]) {
                        matched = 1;
                    } else {
                        matched = 0;
                    }
                }
                if matched == k {
                    record_doc(d, &mut t_cnt, &mut t_sum, &mut top, &mut tc);
                }
            }
        }
        fmt_res(t_cnt, t_sum, &top, tc, out);
    }
}

unsafe fn worker_scan_fw(
    c: usize,
    nt: usize,
    w_per_doc: usize,
    co_ptr: *mut u16,
    doc_ts_ptr: *mut u32,
    g_bs_ptr: *mut u64,
    bs_idx: *const u16,
    d_buf: &[u8],
    doc_s: &[u32; NUM_C],
    c_byte: &[usize; NUM_C + 1],
    fw_ptr: *mut u16,
    dp_ptr: *mut u32,
) {
    let mut last_doc = vec![0u16; nt];
    let (mut p, pe, mut d, mut ld) = (c_byte[c], c_byte[c + 1], doc_s[c], 1u16);
    let mut fw_idx = 0usize;
    while p < pe {
        while *d_buf.get_unchecked(p) != b'\t' {
            p += 1;
        }
        p += 1;
        *doc_ts_ptr.add(d as usize) = p as u32;
        *dp_ptr.add((ld - 1) as usize) = fw_idx as u32;
        let d_w = (d as usize) >> 6;
        let d_b = 1u64 << (d & 63);
        scan_doc(d_buf, &mut p, g_bs_ptr, w_per_doc, d_w, d_b, |t| {
            let bi = *bs_idx.add(t);
            if bi != u16::MAX {
                *g_bs_ptr.add((bi as usize) * w_per_doc + d_w) |= d_b;
            } else if *last_doc.get_unchecked(t) != ld {
                *last_doc.get_unchecked_mut(t) = ld;
                *co_ptr.add(t) += 1;
                if fw_idx + 2 < FW_CAP {
                    if t < 65535 {
                        *fw_ptr.add(fw_idx) = t as u16;
                        fw_idx += 1;
                    } else {
                        *fw_ptr.add(fw_idx) = 65535;
                        *fw_ptr.add(fw_idx + 1) = (t - 65535) as u16;
                        fw_idx += 2;
                    }
                }
            }
        });
        d += 1;
        ld += 1;
    }
    *dp_ptr.add((ld - 1) as usize) = fw_idx as u32;
}

unsafe fn worker_transpose(
    c: usize,
    cur_ptr: *mut u32,
    chunk_u16_ptr: *mut u16,
    doc_s: &[u32; NUM_C],
    doc_e: &[u32; NUM_C],
    fw_ptr: *mut u16,
    dp_ptr: *mut u32,
) {
    let num_docs = (doc_e[c] - doc_s[c]) as usize;
    for ld in 0..num_docs {
        let start = *dp_ptr.add(ld) as usize;
        let end = *dp_ptr.add(ld + 1) as usize;
        let mut i = start;
        while i < end {
            let mut t = *fw_ptr.add(i) as usize;
            i += 1;
            if t == 65535 {
                t = 65535 + *fw_ptr.add(i) as usize;
                i += 1;
            }
            let pos = *cur_ptr.add(t);
            *cur_ptr.add(t) = pos + 1;
            *chunk_u16_ptr.add(pos as usize) = ld as u16;
        }
    }
}

unsafe fn run_all() {
    mallopt(-8, 1);
    mallopt(-1, 0);
    mallopt(-3, 65536);
    let mut args = std::env::args().skip(1);
    let (mut d_dir, mut o_dir) = ("/data".into(), "/out".into());
    while let Some(a) = args.next() {
        if a == "--data" {
            d_dir = args.next().unwrap();
        } else if a == "--out" {
            o_dir = args.next().unwrap();
        }
    }
    let q_buf = map_file(&format!("{}/queries.txt", d_dir));
    let d_buf = map_file(&format!("{}/docs.txt", d_dir));
    let mut queries = Vec::with_capacity(50000);
    let mut q_terms = Vec::with_capacity(160000);
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
        let t_idx = q_terms.len() as u32;
        let mut q = Query {
            ib: (!ib) as u8,
            k: 0,
            ops: 0,
            t_idx,
        };
        for tok in q_buf[qp..le]
            .split(|&b| b == b' ')
            .filter(|s| !s.is_empty())
        {
            let tk = (tok.as_ptr() as usize) - (q_buf.as_ptr() as usize);
            match (ib, tok) {
                (true, b"AND") => q.ops |= 1 << ((q.k - 1) * 2),
                (true, b"OR") => q.ops |= 2 << ((q.k - 1) * 2),
                (true, b"NOT") => q.ops |= 3 << ((q.k - 1) * 2),
                _ => {
                    let tid = insert_term(tok, tk as u32);
                    q_terms.push(tid);
                    q.k += 1;
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
    queries.shrink_to_fit();
    q_terms.shrink_to_fit();
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
    let nt = N_TERMS;
    let w_per_doc = ((total_docs + 63) >> 6) as usize;
    let mut bs_idx = vec![u16::MAX; nt];
    let mut n_bs = 0usize;
    let sample_docs = total_docs.min(600);
    let mut sample_counts = vec![0u16; nt];
    let mut sp = 0usize;
    let mut s_last_doc = vec![0u16; nt];
    for s_ld in 1..=sample_docs as u16 {
        while sp < f_sz && d_buf[sp] != b'\t' {
            sp += 1;
        }
        if sp >= f_sz {
            break;
        }
        sp += 1;
        scan_doc(d_buf, &mut sp, ptr::null_mut(), 0, 0, 0, |t| {
            let last = *s_last_doc.get_unchecked(t);
            if last != s_ld {
                *s_last_doc.get_unchecked_mut(t) = s_ld;
                sample_counts[t] = sample_counts[t].saturating_add(1);
            }
        });
    }
    drop(s_last_doc);
    let threshold = if total_docs >= 25000 { 34 } else { 15 };
    for i in 0..nt {
        if sample_counts[i] >= threshold {
            bs_idx[i] = n_bs as u16;
            n_bs += 1;
            TINFO[i].df = 65536 + (sample_counts[i] as u32) * 500;
        }
    }
    let mut term_order: Vec<u32> = (0..nt as u32).collect();
    term_order.sort_unstable_by_key(|&t| std::cmp::Reverse(sample_counts[t as usize]));
    for &t in &term_order {
        let ti = &TINFO[t as usize];
        if ti.len <= 8 && sample_counts[t as usize] > 0 {
            let w0 = ti.w0;
            let slot = (w0.wrapping_mul(0x9e3779b97f4a7c15) >> 32) as usize & FC_MSK;
            let b = &mut FAST_CACHE[slot];
            if b.e0.w0 == 0 {
                b.e0 = FastEntry {
                    w0,
                    tid: t,
                    bi: bs_idx[t as usize],
                    len: ti.len as u8,
                    _pad: 0,
                };
            } else if b.e1.w0 == 0 {
                b.e1 = FastEntry {
                    w0,
                    tid: t,
                    bi: bs_idx[t as usize],
                    len: ti.len as u8,
                    _pad: 0,
                };
            }
        }
    }
    drop(sample_counts);
    let g_bs_total = n_bs * w_per_doc;
    let raw_bs_ptr = alloc_mmap::<u64>(g_bs_total);
    let raw_bs_val = raw_bs_ptr as usize;
    let mut doc_ts = vec![0u32; total_docs as usize];
    let dpc = (total_docs + NUM_C as u32 - 1) / NUM_C as u32;
    let (mut doc_s, mut doc_e, mut c_byte) = ([0u32; NUM_C], [0u32; NUM_C], [0usize; NUM_C + 1]);
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
    let sc_count = NUM_C * nt;
    let c_offs = alloc_mmap::<u32>(sc_count);
    let c_offs_val = c_offs as usize;
    let thread_counts = alloc_mmap::<u16>(NUM_T * nt);
    let thread_counts_val = thread_counts as usize;
    let doc_ts_ptr = doc_ts.as_mut_ptr() as usize;
    const ATOMIC_ZERO: AtomicUsize = AtomicUsize::new(0);
    let c_post_ptrs: [AtomicUsize; NUM_C] = [ATOMIC_ZERO; NUM_C];
    let c_post_lens: [AtomicUsize; NUM_C] = [ATOMIC_ZERO; NUM_C];
    let bs_idx_ptr = bs_idx.as_ptr() as usize;
    const QB: usize = 128;
    let nb = (queries.len() + QB - 1) / QB;
    let mut out_bufs: Vec<Vec<u8>> = (0..nb).map(|_| Vec::with_capacity(QB * 30)).collect();
    let out_bufs_ptr = out_bufs.as_mut_ptr() as usize;
    let next_b = AtomicUsize::new(0);
    let mut fw_ptrs = [0usize; NUM_T];
    let mut dp_ptrs = [0usize; NUM_T];
    for t in 0..NUM_T {
        fw_ptrs[t] = alloc_mmap::<u16>(FW_CAP) as usize;
        dp_ptrs[t] = alloc_mmap::<u32>(DP_CAP) as usize;
    }
    let next_chunk = AtomicUsize::new(0);
    let barrier1 = std::sync::Barrier::new(NUM_T);
    let barrier2 = std::sync::Barrier::new(NUM_T);
    std::thread::scope(|s| {
        let do_worker = |tid: usize| {
            let aff_mask = 1u64 << (tid % 12);
            sched_setaffinity(0, 8, &aff_mask);
            let fw = fw_ptrs[tid] as *mut u16;
            let dp = dp_ptrs[tid] as *mut u32;
            let thread_cnt_p = (thread_counts_val as *mut u16).add(tid * nt);
            let bs_idx_p = bs_idx_ptr as *const u16;
            loop {
                let c = next_chunk.fetch_add(1, Ordering::Relaxed);
                if c >= NUM_C {
                    break;
                }
                ptr::write_bytes(thread_cnt_p, 0, nt);
                worker_scan_fw(
                    c,
                    nt,
                    w_per_doc,
                    thread_cnt_p,
                    doc_ts_ptr as *mut u32,
                    raw_bs_val as *mut u64,
                    bs_idx_p,
                    d_buf,
                    &doc_s,
                    &c_byte,
                    fw,
                    dp,
                );
                let c_off_p = (c_offs_val as *mut u32).add(c * nt);
                let mut my_tot = 0u32;
                for i in 0..nt {
                    *c_off_p.add(i) = my_tot;
                    if *bs_idx_p.add(i) == u16::MAX {
                        let cnt = *thread_cnt_p.add(i) as u32;
                        my_tot += cnt;
                        if cnt != 0 {
                            GLOBAL_DF[i].fetch_add(cnt, Ordering::Relaxed);
                        }
                    }
                }
                let my_total = (my_tot as usize).max(1);
                let chunk_u16 = alloc_mmap::<u16>(my_total);
                c_post_ptrs[c].store(chunk_u16 as usize, Ordering::Release);
                c_post_lens[c].store(my_total, Ordering::Relaxed);
                worker_transpose(c, c_off_p, chunk_u16, &doc_s, &doc_e, fw, dp);
            }
            free_mmap(fw, FW_CAP);
            free_mmap(dp, DP_CAP);
            barrier1.wait();
            let chunk_nt = (nt + NUM_T - 1) / NUM_T;
            let t_start = tid * chunk_nt;
            let t_end = (t_start + chunk_nt).min(nt);
            for i in t_start..t_end {
                if *bs_idx_p.add(i) == u16::MAX {
                    TINFO[i].df = GLOBAL_DF[i].load(Ordering::Relaxed);
                }
            }
            barrier2.wait();
            if tid == 0 {
                madvise(
                    VTAB.as_mut_ptr() as *mut u8,
                    std::mem::size_of_val(&VTAB),
                    4,
                );
                free_mmap(thread_counts_val as *mut u16, NUM_T * nt);
            }
            let mut cp_ptrs = [ptr::null(); NUM_C];
            for c in 0..NUM_C {
                cp_ptrs[c] = c_post_ptrs[c].load(Ordering::Relaxed) as *const u16;
            }
            let bi_slice = std::slice::from_raw_parts(bs_idx_ptr as *const u16, nt);
            let c_off_slice = std::slice::from_raw_parts(c_offs_val as *const u32, NUM_C * nt);
            let g_bs_slice = std::slice::from_raw_parts(raw_bs_val as *const u64, g_bs_total);
            let doc_ts_slice =
                std::slice::from_raw_parts(doc_ts_ptr as *const u32, total_docs as usize);
            let eng = Engine {
                c_offs: c_off_slice,
                c_postings: cp_ptrs,
                bs_idx: bi_slice,
                doc_s: &doc_s,
                doc_e: &doc_e,
                w_per_doc,
                g_bs: g_bs_slice,
                doc_ts: doc_ts_slice,
                d_buf,
                q_buf,
                q_terms: &q_terms,
                nt,
            };
            loop {
                let b = next_b.fetch_add(1, Ordering::Relaxed);
                if b >= nb {
                    break;
                }
                let ob = &mut *(out_bufs_ptr as *mut Vec<u8>).add(b);
                for q in &queries[b * QB..((b + 1) * QB).min(queries.len())] {
                    if q.ib == 0 {
                        eng.exec_bool(q, ob);
                    } else {
                        eng.exec_phrase(q, ob);
                    }
                }
            }
        };
        for tid in 1..NUM_T {
            std::thread::Builder::new()
                .stack_size(196608)
                .spawn_scoped(s, move || do_worker(tid))
                .unwrap();
        }
        do_worker(0);
    });
    let _ = std::fs::create_dir_all(&o_dir);
    let f = File::create(format!("{}/results.txt", o_dir)).unwrap();
    let mut writer = std::io::BufWriter::with_capacity(262144, f);
    for ob in &out_bufs {
        writer.write_all(ob).unwrap();
    }
    writer.flush().unwrap();
    _exit(0);
}

fn main() {
    unsafe {
        run_all();
    }
}
