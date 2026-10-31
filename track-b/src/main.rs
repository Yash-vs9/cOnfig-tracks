#![allow(static_mut_refs)]
use std::{
    arch::x86_64::*,
    fs::File,
    io::Write,
    mem::MaybeUninit,
    os::unix::io::IntoRawFd,
    ptr,
    sync::atomic::{AtomicUsize, Ordering},
};

extern "C" {
    fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
    fn munmap(a: *mut u8, l: usize) -> i32;
    fn close(fd: i32) -> i32;
    fn mallopt(p: i32, v: i32) -> i32;
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
    fn malloc_trim(pad: usize) -> i32;
    fn sched_setaffinity(pid: i32, cpusetsize: usize, mask: *const u64) -> i32;
}

const H_SZ: usize = 131072;
const H_MSK: usize = H_SZ - 1;
const NUM_T: usize = 12;
const CHUNK_DOCS: usize = 3000;
const FW_CAP: usize = 350_000;
const DP_CAP: usize = 3016;
const MAX_TERMS: usize = 81920;

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
    w1: u64,
    off: u32,
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
struct QueryAcc {
    sum: u64,
    cnt: u32,
    head_len: u32, // (head_slot << 8) | (len & 0xFF)
}

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

const MASKS: [u64; 9] = [
    0,
    0x00000000000000FF,
    0x000000000000FFFF,
    0x0000000000FFFFFF,
    0x00000000FFFFFFFF,
    0x000000FFFFFFFFFF,
    0x0000FFFFFFFFFFFF,
    0x00FFFFFFFFFFFFFF,
    0xFFFFFFFFFFFFFFFF,
];

#[derive(Clone, Copy, Default)]
struct FastEntry {
    w0: u64,
    tid: u32,
    len: u16,
}

#[repr(align(4096))]
struct AlignedVTab([Slot; H_SZ]);
static mut VTAB: AlignedVTab = AlignedVTab([Slot { w0: 0, h_tag: 0, tid: 0 }; H_SZ]);
static mut TINFO: [TInfo; MAX_TERMS] = [TInfo { w0: 0, w1: 0, off: 0, len: 0 }; MAX_TERMS];
static mut N_TERMS: usize = 0;
const PF_BITS: usize = 2097152;
const PF_MASK: usize = PF_BITS - 1;
static mut PF: [u64; 32768] = [0; 32768];

#[inline(always)]
fn mix64(w0: u64, w1: u64, len: usize) -> u64 {
    let h = if len <= 8 {
        (w0 ^ (len as u64)).wrapping_mul(0xbf58476d1ce4e5b9)
    } else {
        (w0 ^ (w1.wrapping_mul(0x9e3779b97f4a7c15).wrapping_add(len as u64)))
            .wrapping_mul(0xbf58476d1ce4e5b9)
    };
    h ^ (h >> 31)
}

#[inline(always)]
unsafe fn lookup_swar(w0: u64, w1: u64, tail: &[u8], len: usize, q_buf: &[u8], h: u64) -> (u32, usize) {
    let (h32, mut idx) = (((h ^ (h >> 32)) as u32) | 1, (h as usize) & H_MSK);
    let h_tag = (h32 & 0x00FFFFFF) | ((len as u32) << 24);
    loop {
        let s = VTAB.0.get_unchecked(idx);
        if s.h_tag == 0 {
            return (u32::MAX, idx);
        }
        if s.h_tag == h_tag && s.w0 == w0 {
            if len <= 8 {
                return (s.tid, idx);
            }
            let ti = TINFO.get_unchecked(s.tid as usize);
            if len <= 16 {
                if ti.w1 == w1 {
                    return (s.tid, idx);
                }
            } else if ti.w1 == w1
                && std::slice::from_raw_parts(q_buf.as_ptr().add(ti.off as usize + 16), len - 16)
                    == &tail[..len - 16]
            {
                return (s.tid, idx);
            }
        }
        idx = (idx + 1) & H_MSK;
    }
}

unsafe fn insert_term(s: &[u8], off: u32, q_buf: &[u8]) -> u32 {
    let (mut w0, mut w1, len) = (0u64, 0u64, s.len());
    ptr::copy_nonoverlapping(s.as_ptr(), &mut w0 as *mut _ as *mut u8, len.min(8));
    if len > 8 {
        ptr::copy_nonoverlapping(s.as_ptr().add(8), &mut w1 as *mut _ as *mut u8, (len - 8).min(8));
    }
    let h = mix64(w0, w1, len);
    let tail = if len > 16 { &s[16..] } else { &[] };
    let (tid, empty_idx) = lookup_swar(w0, w1, tail, len, q_buf, h);
    if tid != u32::MAX {
        return tid;
    }
    let h32 = ((h ^ (h >> 32)) as u32) | 1;
    let h_tag = (h32 & 0x00FFFFFF) | ((len as u32) << 24);
    let nt = N_TERMS as u32;
    N_TERMS += 1;
    VTAB.0[empty_idx] = Slot { w0, h_tag, tid: nt };
    TINFO[nt as usize] = TInfo { w0, w1, off, len: len as u16 };
    let pb = ((h ^ (h >> 32)) as usize) & PF_MASK;
    PF[pb >> 6] |= 1u64 << (pb & 63);
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
unsafe fn tokmask(c: __m256i) -> u32 {
    let lo = _mm256_or_si256(c, _mm256_set1_epi8(0x20));
    let t = _mm256_sub_epi8(lo, _mm256_set1_epi8(0x61));
    let l = _mm256_cmpeq_epi8(_mm256_min_epu8(t, _mm256_set1_epi8(25)), t);
    let d = _mm256_sub_epi8(c, _mm256_set1_epi8(0x30));
    let n = _mm256_cmpeq_epi8(_mm256_min_epu8(d, _mm256_set1_epi8(9)), d);
    _mm256_movemask_epi8(_mm256_or_si256(l, n)) as u32
}

#[target_feature(enable = "avx2,bmi1,bmi2,lzcnt,popcnt")]
#[inline]
unsafe fn scan_doc<F: FnMut(usize)>(
    d_buf: &[u8],
    p: &mut usize,
    q_buf: &[u8],
    rep_map: &[u32],
    fc: &mut [[FastEntry; 2]; 1024],
    mut fn_: F,
) {
    let (base, n) = (d_buf.as_ptr(), d_buf.len());
    let nlv = _mm256_set1_epi8(b'\n' as i8);
    let (mut pos, mut p_tid, mut p_pos) = (0u32, u32::MAX, u32::MAX);
    let mut tail = [0u8; 16];
    let mut process_token = |w0: u64, w1: u64, len: usize, tail: &[u8]| {
        let tid = if len <= 8 {
            let fi = ((w0 ^ (w0 >> 17)) as usize) & 1023;
            let set = fc.get_unchecked_mut(fi);
            if set[0].w0 == w0 && set[0].len == len as u16 {
                set[0].tid
            } else if set[1].w0 == w0 && set[1].len == len as u16 {
                let hit = set[1];
                set[1] = set[0];
                set[0] = hit;
                hit.tid
            } else {
                let h = mix64(w0, w1, len);
                let pb = ((h ^ (h >> 32)) as usize) & PF_MASK;
                let t = if (*PF.get_unchecked(pb >> 6) & (1u64 << (pb & 63))) == 0 {
                    u32::MAX
                } else {
                    lookup_swar(w0, w1, tail, len, q_buf, h).0
                };
                set[1] = set[0];
                set[0] = FastEntry { w0, tid: t, len: len as u16 };
                t
            }
        } else {
            let h = mix64(w0, w1, len);
            let pb = ((h ^ (h >> 32)) as usize) & PF_MASK;
            if (*PF.get_unchecked(pb >> 6) & (1u64 << (pb & 63))) == 0 {
                u32::MAX
            } else {
                lookup_swar(w0, w1, tail, len, q_buf, h).0
            }
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
    };
    while *p + 32 <= n {
        let c = _mm256_loadu_si256(base.add(*p) as *const __m256i);
        let mut mask_tok = tokmask(c);
        let mask_nl = _mm256_movemask_epi8(_mm256_cmpeq_epi8(c, nlv)) as u32;
        let nl_pos = if mask_nl != 0 { mask_nl.trailing_zeros() as usize } else { 32 };
        if mask_nl != 0 {
            mask_tok &= (1u32 << nl_pos).wrapping_sub(1);
        }
        let mut offset = 0usize;
        let mut cross = false;
        while mask_tok != 0 {
            let zeros = mask_tok.trailing_zeros() as usize;
            let tok_start = offset + zeros;
            let rem = mask_tok >> zeros;
            let len = (!rem).trailing_zeros() as usize;
            if mask_nl == 0 && tok_start + len == 32 {
                *p += tok_start;
                cross = true;
                break;
            }
            let (w0, w1);
            if len <= 8 {
                let raw = std::ptr::read_unaligned(base.add(*p + tok_start) as *const u64) | 0x2020202020202020;
                w0 = raw & *MASKS.get_unchecked(len);
                w1 = 0;
                process_token(w0, w1, len, &[]);
            } else if len <= 16 {
                w0 = std::ptr::read_unaligned(base.add(*p + tok_start) as *const u64) | 0x2020202020202020;
                let raw1 = std::ptr::read_unaligned(base.add(*p + tok_start + 8) as *const u64) | 0x2020202020202020;
                w1 = raw1 & *MASKS.get_unchecked(len - 8);
                process_token(w0, w1, len, &[]);
            } else {
                let mut tp = *p + tok_start;
                let r = read_tok(d_buf, &mut tp, &mut tail);
                w0 = r.0;
                w1 = r.1;
                process_token(w0, w1, len, &tail);
            }
            let consumed = zeros + len;
            if consumed >= 32 {
                break;
            }
            mask_tok = rem >> len;
            offset = tok_start + len;
        }
        if mask_nl != 0 {
            *p += nl_pos + 1;
            return;
        }
        if cross {
            let l = (!tokmask(_mm256_loadu_si256(base.add(*p) as *const __m256i))).trailing_zeros() as usize;
            if l >= 16 || *p + 32 > n {
                let r = read_tok(d_buf, p, &mut tail);
                process_token(r.0, r.1, r.2, &tail);
            }
        } else {
            *p += 32;
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
        let r = read_tok(d_buf, p, &mut tail);
        process_token(r.0, r.1, r.2, &tail);
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
    let ptr = mmap(ptr::null_mut(), len, 1, 2, fd, 0);
    close(fd);
    std::slice::from_raw_parts(ptr, len)
}

unsafe fn alloc_mmap<T>(count: usize) -> *mut T {
    let bytes = count * size_of::<T>();
    mmap(ptr::null_mut(), bytes, 3, 34, -1, 0) as *mut T
}

unsafe fn free_mmap<T>(p: *mut T, count: usize) {
    if !p.is_null() && count > 0 {
        munmap(p as *mut u8, count * size_of::<T>());
    }
}

#[inline(always)]
unsafe fn record_match(acc: &mut QueryAcc, top_arena: &mut Vec<u32>, global_id: u32) {
    acc.cnt += 1;
    acc.sum += global_id as u64;
    let cur_len = (acc.head_len & 0xFF) as usize;
    if cur_len < 10 {
        let slot = if cur_len == 0 {
            let sl = (top_arena.len() / 10) as u32;
            top_arena.extend_from_slice(&[0u32; 10]);
            acc.head_len = sl << 8;
            sl as usize
        } else {
            (acc.head_len >> 8) as usize
        };
        *top_arena.get_unchecked_mut(slot * 10 + cur_len) = global_id;
        acc.head_len += 1;
    }
}

#[inline(always)]
unsafe fn record_batch(acc: &mut QueryAcc, top_arena: &mut Vec<u32>, doc_s: u32, clist: &[u16]) {
    let clen = clist.len();
    if clen == 0 {
        return;
    }
    acc.cnt += clen as u32;
    let mut s = 0u64;
    for &ld in clist {
        s += ld as u64;
    }
    acc.sum += (doc_s as u64) * (clen as u64) + s;
    let cur_len = (acc.head_len & 0xFF) as usize;
    let need = (10 - cur_len).min(clen);
    if need > 0 {
        let slot = if cur_len == 0 {
            let sl = (top_arena.len() / 10) as u32;
            top_arena.extend_from_slice(&[0u32; 10]);
            acc.head_len = sl << 8;
            sl as usize
        } else {
            (acc.head_len >> 8) as usize
        };
        let head = slot * 10 + cur_len;
        for i in 0..need {
            *top_arena.get_unchecked_mut(head + i) = doc_s + *clist.get_unchecked(i) as u32;
        }
        acc.head_len += need as u32;
    }
}

#[inline(always)]
unsafe fn record_bits(acc: &mut QueryAcc, top_arena: &mut Vec<u32>, doc_s: u32, curr: &[u64], cd: u32, cw: usize) {
    for w in 0..cw {
        let mut val = *curr.get_unchecked(w);
        if w == (cd >> 6) as usize {
            let rem = cd & 63;
            if rem != 0 {
                val &= (1u64 << rem) - 1;
            }
        }
        if val == 0 {
            continue;
        }
        let base_id = doc_s + (w << 6) as u32;
        let cnt = val.count_ones();
        acc.cnt += cnt;
        acc.sum += (cnt as u64) * (base_id as u64);
        while val != 0 {
            let bit = val.trailing_zeros();
            acc.sum += bit as u64;
            let cur_len = (acc.head_len & 0xFF) as usize;
            if cur_len < 10 {
                let slot = if cur_len == 0 {
                    let sl = (top_arena.len() / 10) as u32;
                    top_arena.extend_from_slice(&[0u32; 10]);
                    acc.head_len = sl << 8;
                    sl as usize
                } else {
                    (acc.head_len >> 8) as usize
                };
                *top_arena.get_unchecked_mut(slot * 10 + cur_len) = base_id + bit;
                acc.head_len += 1;
            }
            val &= val - 1;
        }
    }
}

#[inline(always)]
unsafe fn intersect_and(
    t: usize,
    chunk_postings: *const u16,
    cur_pos: &[u32],
    term_counts: &[u16],
    chunk_bs_idx: &[u16],
    chunk_bs: &[u64],
    cw: usize,
    cand: &mut [u16],
    clen: usize,
) -> usize {
    let bi = *chunk_bs_idx.get_unchecked(t);
    if bi != u16::MAX {
        let src = &chunk_bs[(bi as usize) * cw..(bi as usize + 1) * cw];
        let mut w = 0usize;
        for i in 0..clen {
            let ld = *cand.get_unchecked(i) as usize;
            if (*src.get_unchecked(ld >> 6) & (1u64 << (ld & 63))) != 0 {
                *cand.get_unchecked_mut(w) = ld as u16;
                w += 1;
            }
        }
        w
    } else {
        let cnt = *term_counts.get_unchecked(t) as usize;
        if cnt == 0 {
            return 0;
        }
        let end = *cur_pos.get_unchecked(t) as usize;
        let o_post = std::slice::from_raw_parts(chunk_postings.add(end - cnt), cnt);
        let mut w = 0usize;
        let (mut i, mut j) = (0usize, 0usize);
        while i < clen && j < cnt {
            let (v1, v2) = (*cand.get_unchecked(i), *o_post.get_unchecked(j));
            if v1 == v2 {
                *cand.get_unchecked_mut(w) = v1;
                w += 1;
                i += 1;
                j += 1;
            } else if v1 < v2 {
                i += 1;
            } else {
                j += 1;
            }
        }
        w
    }
}

#[inline(always)]
unsafe fn intersect_not(
    t: usize,
    chunk_postings: *const u16,
    cur_pos: &[u32],
    term_counts: &[u16],
    chunk_bs_idx: &[u16],
    chunk_bs: &[u64],
    cw: usize,
    cand: &mut [u16],
    clen: usize,
) -> usize {
    let bi = *chunk_bs_idx.get_unchecked(t);
    if bi != u16::MAX {
        let src = &chunk_bs[(bi as usize) * cw..(bi as usize + 1) * cw];
        let mut w = 0usize;
        for i in 0..clen {
            let ld = *cand.get_unchecked(i) as usize;
            if (*src.get_unchecked(ld >> 6) & (1u64 << (ld & 63))) == 0 {
                *cand.get_unchecked_mut(w) = ld as u16;
                w += 1;
            }
        }
        w
    } else {
        let cnt = *term_counts.get_unchecked(t) as usize;
        if cnt == 0 {
            return clen;
        }
        let end = *cur_pos.get_unchecked(t) as usize;
        let o_post = std::slice::from_raw_parts(chunk_postings.add(end - cnt), cnt);
        let mut w = 0usize;
        let (mut i, mut j) = (0usize, 0usize);
        while i < clen && j < cnt {
            let (v1, v2) = (*cand.get_unchecked(i), *o_post.get_unchecked(j));
            if v1 == v2 {
                i += 1;
                j += 1;
            } else if v1 < v2 {
                *cand.get_unchecked_mut(w) = v1;
                w += 1;
                i += 1;
            } else {
                j += 1;
            }
        }
        while i < clen {
            *cand.get_unchecked_mut(w) = *cand.get_unchecked(i);
            w += 1;
            i += 1;
        }
        w
    }
}

#[target_feature(enable = "avx2,bmi1,bmi2,lzcnt,popcnt")]
unsafe fn exec_bool_chunk(
    q: &Query,
    q_ts: &[u32],
    doc_s: u32,
    cd: u32,
    cw: usize,
    cur_pos: &[u32],
    term_counts: &[u16],
    chunk_bs_idx: &[u16],
    chunk_bs: &[u64],
    chunk_postings: *const u16,
    cand: &mut [u16],
    curr: &mut [u64],
    acc: &mut QueryAcc,
    top_arena: &mut Vec<u32>,
) {
    let is_pure_and = (1..q.k as usize).all(|i| ((q.ops >> ((i - 1) * 2)) & 3) == 1);
    if is_pure_and {
        let mut min_len = usize::MAX;
        let mut min_t = usize::MAX;
        for &tid in q_ts {
            let t = tid as usize;
            let l = *term_counts.get_unchecked(t) as usize;
            if l == 0 {
                return;
            }
            if *chunk_bs_idx.get_unchecked(t) == u16::MAX && l < min_len {
                min_len = l;
                min_t = t;
            }
        }
        if min_t != usize::MAX {
            let end0 = *cur_pos.get_unchecked(min_t) as usize;
            cand[..min_len].copy_from_slice(std::slice::from_raw_parts(chunk_postings.add(end0 - min_len), min_len));
            let mut clen = min_len;
            for &tid in q_ts {
                let t = tid as usize;
                if t != min_t {
                    clen = intersect_and(t, chunk_postings, cur_pos, term_counts, chunk_bs_idx, chunk_bs, cw, cand, clen);
                    if clen == 0 {
                        return;
                    }
                }
            }
            record_batch(acc, top_arena, doc_s, &cand[..clen]);
            return;
        } else {
            let b0 = *chunk_bs_idx.get_unchecked(q_ts[0] as usize) as usize;
            curr[..cw].copy_from_slice(&chunk_bs[b0 * cw..(b0 + 1) * cw]);
            for &tid in &q_ts[1..] {
                let bt = *chunk_bs_idx.get_unchecked(tid as usize) as usize;
                let src = &chunk_bs[bt * cw..(bt + 1) * cw];
                for w in 0..cw {
                    *curr.get_unchecked_mut(w) &= *src.get_unchecked(w);
                }
            }
            record_bits(acc, top_arena, doc_s, curr, cd, cw);
            return;
        }
    }

    let mut in_bs = false;
    let mut clen = 0usize;
    let t0 = q_ts[0] as usize;
    let b0 = *chunk_bs_idx.get_unchecked(t0);
    if b0 != u16::MAX {
        in_bs = true;
        curr[..cw].copy_from_slice(&chunk_bs[(b0 as usize) * cw..(b0 as usize + 1) * cw]);
    } else {
        let cnt0 = *term_counts.get_unchecked(t0) as usize;
        if cnt0 > 0 {
            let end0 = *cur_pos.get_unchecked(t0) as usize;
            cand[..cnt0].copy_from_slice(std::slice::from_raw_parts(chunk_postings.add(end0 - cnt0), cnt0));
            clen = cnt0;
        }
    }

    for i in 0..(q.k - 1) as usize {
        let op = ((q.ops >> (i * 2)) & 3) as u8;
        let nt_id = q_ts[i + 1] as usize;
        let n_bi = *chunk_bs_idx.get_unchecked(nt_id);
        if !in_bs && op == 2 {
            curr[..cw].fill(0);
            for &ld in &cand[..clen] {
                *curr.get_unchecked_mut((ld as usize) >> 6) |= 1u64 << (ld & 63);
            }
            in_bs = true;
        }
        if in_bs {
            if n_bi != u16::MAX {
                let src = &chunk_bs[(n_bi as usize) * cw..(n_bi as usize + 1) * cw];
                let mut w = 0usize;
                while w + 4 <= cw {
                    let c_v = _mm256_loadu_si256(curr.as_ptr().add(w) as *const __m256i);
                    let s_v = _mm256_loadu_si256(src.as_ptr().add(w) as *const __m256i);
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
            } else {
                let n_cnt = *term_counts.get_unchecked(nt_id) as usize;
                let end_n = *cur_pos.get_unchecked(nt_id) as usize;
                let n_post = if n_cnt > 0 {
                    std::slice::from_raw_parts(chunk_postings.add(end_n - n_cnt), n_cnt)
                } else {
                    &[]
                };
                if op == 1 {
                    let mut w = 0usize;
                    for &ld in n_post {
                        if (*curr.get_unchecked((ld as usize) >> 6) & (1u64 << (ld & 63))) != 0 {
                            *cand.get_unchecked_mut(w) = ld;
                            w += 1;
                        }
                    }
                    clen = w;
                    in_bs = false;
                } else if op == 2 {
                    for &ld in n_post {
                        *curr.get_unchecked_mut((ld as usize) >> 6) |= 1u64 << (ld & 63);
                    }
                } else {
                    for &ld in n_post {
                        *curr.get_unchecked_mut((ld as usize) >> 6) &= !(1u64 << (ld & 63));
                    }
                }
            }
        } else {
            if op == 1 {
                clen = intersect_and(nt_id, chunk_postings, cur_pos, term_counts, chunk_bs_idx, chunk_bs, cw, cand, clen);
            } else if op == 3 {
                clen = intersect_not(nt_id, chunk_postings, cur_pos, term_counts, chunk_bs_idx, chunk_bs, cw, cand, clen);
            }
        }
    }
    if !in_bs {
        record_batch(acc, top_arena, doc_s, &cand[..clen]);
    } else {
        record_bits(acc, top_arena, doc_s, curr, cd, cw);
    }
}

#[target_feature(enable = "avx2,bmi1,bmi2,lzcnt,popcnt")]
unsafe fn exec_phrase_chunk(
    q: &Query,
    q_terms: &[u32],
    rep_map: &[u32],
    doc_s: u32,
    _cd: u32,
    cw: usize,
    cur_pos: &[u32],
    term_counts: &[u16],
    chunk_bs_idx: &[u16],
    chunk_bs: &[u64],
    chunk_postings: *const u16,
    doc_ts: &[u32],
    d_buf: &[u8],
    q_buf: &[u8],
    cand: &mut [u16],
    acc: &mut QueryAcc,
    top_arena: &mut Vec<u32>,
) {
    let k = q.k as usize;
    let mut nd = 0usize;
    let mut dist = [0usize; 16];
    let q_ts = &q_terms[q.t_idx as usize..q.t_idx as usize + k];

    for i in 0..k {
        let t = q_ts[i] as usize;
        if *term_counts.get_unchecked(t) == 0 {
            return;
        }
        if !dist[..nd].contains(&t) {
            dist[nd] = t;
            nd += 1;
        }
        if i + 1 < k && t == q_ts[i + 1] as usize && t < rep_map.len() {
            let rt = *rep_map.get_unchecked(t) as usize;
            if rt != u32::MAX as usize {
                if *term_counts.get_unchecked(rt) == 0 {
                    return;
                }
                if !dist[..nd].contains(&rt) {
                    dist[nd] = rt;
                    nd += 1;
                }
            }
        }
    }

    dist[..nd].sort_unstable_by_key(|&t| *term_counts.get_unchecked(t));

    let rare = dist[0];
    let r_cnt = *term_counts.get_unchecked(rare) as usize;
    let r_bi = *chunk_bs_idx.get_unchecked(rare);
    let mut clen = 0usize;
    if r_bi == u16::MAX {
        let end = *cur_pos.get_unchecked(rare) as usize;
        cand[..r_cnt].copy_from_slice(std::slice::from_raw_parts(chunk_postings.add(end - r_cnt), r_cnt));
        clen = r_cnt;
    } else {
        let src = &chunk_bs[(r_bi as usize) * cw..(r_bi as usize + 1) * cw];
        for w in 0..cw {
            let mut v = *src.get_unchecked(w);
            while v != 0 {
                let id = (w << 6) + v.trailing_zeros() as usize;
                cand[clen] = id as u16;
                clen += 1;
                v &= v - 1;
            }
        }
    }
    if clen == 0 {
        return;
    }

    for &ot in &dist[1..nd] {
        clen = intersect_and(ot, chunk_postings, cur_pos, term_counts, chunk_bs_idx, chunk_bs, cw, cand, clen);
        if clen == 0 {
            return;
        }
    }

    let mut pw0 = [0u64; 8];
    let mut pw1 = [0u64; 8];
    let mut plen = [0usize; 8];
    let mut poff = [0usize; 8];
    for i in 0..k {
        let ti = *TINFO.get_unchecked(q_ts[i] as usize);
        pw0[i] = ti.w0;
        pw1[i] = ti.w1;
        plen[i] = ti.len as usize;
        poff[i] = ti.off as usize;
    }
    let first_ch = (pw0[0] & 0xFF) as u8;
    let first_up = if first_ch >= b'a' && first_ch <= b'z' { first_ch - 32 } else { first_ch };
    let v_lo = _mm256_set1_epi8(first_ch as i8);
    let v_up = _mm256_set1_epi8(first_up as i8);
    let v_nl = _mm256_set1_epi8(b'\n' as i8);
    let d_ptr = d_buf.as_ptr();
    let d_len = d_buf.len();

    let is_alnum = |b: u8| -> bool { b.wrapping_sub(b'0') <= 9 || (b | 0x20).wrapping_sub(b'a') <= 25 };

    let try_match = |cand_p: usize| -> bool {
        let mut tp = cand_p;
        for j in 0..k {
            let l = plen[j];
            if tp + l >= d_len {
                return false;
            }
            if l <= 8 {
                if tp + 8 > d_len {
                    return false;
                }
                let raw = ptr::read_unaligned(d_ptr.add(tp) as *const u64) | 0x2020202020202020;
                let mask = if l == 8 { u64::MAX } else { (1u64 << (l * 8)) - 1 };
                if (raw & mask) != pw0[j] {
                    return false;
                }
            } else if l <= 16 {
                if tp + 16 > d_len {
                    return false;
                }
                let raw0 = ptr::read_unaligned(d_ptr.add(tp) as *const u64) | 0x2020202020202020;
                if raw0 != pw0[j] {
                    return false;
                }
                let raw1 = ptr::read_unaligned(d_ptr.add(tp + 8) as *const u64) | 0x2020202020202020;
                let mask1 = if l == 16 { u64::MAX } else { (1u64 << ((l - 8) * 8)) - 1 };
                if (raw1 & mask1) != pw1[j] {
                    return false;
                }
            } else {
                if tp + 16 > d_len {
                    return false;
                }
                let raw0 = ptr::read_unaligned(d_ptr.add(tp) as *const u64) | 0x2020202020202020;
                if raw0 != pw0[j] {
                    return false;
                }
                let raw1 = ptr::read_unaligned(d_ptr.add(tp + 8) as *const u64) | 0x2020202020202020;
                if raw1 != pw1[j] {
                    return false;
                }
                let q_tail = &q_buf[poff[j] + 16..poff[j] + l];
                let d_tail = std::slice::from_raw_parts(d_ptr.add(tp + 16), l - 16);
                for t_idx in 0..l - 16 {
                    if (d_tail[t_idx] | 0x20) != q_tail[t_idx] {
                        return false;
                    }
                }
            }
            if is_alnum(*d_ptr.add(tp + l)) {
                return false;
            }
            tp += l;
            if j + 1 < k {
                if *d_ptr.add(tp) == b' ' && is_alnum(*d_ptr.add(tp + 1)) {
                    tp += 1;
                } else {
                    while !is_alnum(*d_ptr.add(tp)) {
                        if *d_ptr.add(tp) == b'\n' {
                            return false;
                        }
                        tp += 1;
                    }
                }
            }
        }
        true
    };

    for ci in 0..clen {
        let ld = *cand.get_unchecked(ci) as usize;
        let mut sp = *doc_ts.get_unchecked(ld) as usize;
        let mut matched = false;
        'doc_scan: loop {
            if sp + 32 <= d_len {
                let chunk = _mm256_loadu_si256(d_ptr.add(sp) as *const __m256i);
                let m_ch = _mm256_or_si256(_mm256_cmpeq_epi8(chunk, v_lo), _mm256_cmpeq_epi8(chunk, v_up));
                let m_nl = _mm256_cmpeq_epi8(chunk, v_nl);
                let (mask_ch, mask_nl) = (_mm256_movemask_epi8(m_ch) as u32, _mm256_movemask_epi8(m_nl) as u32);
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
                    if !is_alnum(*d_ptr.add(cand_p - 1)) && try_match(cand_p) {
                        matched = true;
                        break 'doc_scan;
                    }
                }
                if nl_pos < 32 {
                    break 'doc_scan;
                }
                sp += 32;
            } else {
                while *CMAP.get_unchecked(*d_buf.get_unchecked(sp) as usize) == 0 {
                    sp += 1;
                }
                if *d_buf.get_unchecked(sp) == b'\n' {
                    break 'doc_scan;
                }
                if try_match(sp) {
                    matched = true;
                    break 'doc_scan;
                }
                let mut tail = [0u8; 16];
                read_tok(d_buf, &mut sp, &mut tail);
            }
        }
        if matched {
            let global_id = doc_s + *cand.get_unchecked(ci) as u32;
            record_match(acc, top_arena, global_id);
        }
    }
}

unsafe fn run_all() {
    let t0 = std::time::Instant::now();
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
        let mut q = Query { ib: (!ib) as u8, k: 0, ops: 0, t_idx };
        let mut tp = qp;
        while tp < le {
            while tp < le && q_buf[tp] == b' ' {
                tp += 1;
            }
            if tp >= le {
                break;
            }
            let ts = tp;
            while tp < le && q_buf[tp] != b' ' {
                tp += 1;
            }
            let tok = &q_buf[ts..tp];
            match (ib, tok) {
                (true, b"AND") => q.ops |= 1 << ((q.k - 1) * 2),
                (true, b"OR") => q.ops |= 2 << ((q.k - 1) * 2),
                (true, b"NOT") => q.ops |= 3 << ((q.k - 1) * 2),
                _ => {
                    let tid = insert_term(tok, ts as u32, q_buf);
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
    eprintln!("[STAGE 1] parse: {:?}", t0.elapsed());

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
    while pl > 0 && d_buf[pl - 1] != b'\n' {
        pl -= 1;
    }
    let mut total_docs = 0u32;
    while pl < f_sz && d_buf[pl] >= b'0' && d_buf[pl] <= b'9' {
        total_docs = total_docs * 10 + (d_buf[pl] - b'0') as u32;
        pl += 1;
    }
    total_docs += 1;

    let num_chunks = (total_docs as usize + CHUNK_DOCS - 1) / CHUNK_DOCS;
    let mut c_byte = vec![0usize; num_chunks + 1];
    c_byte[num_chunks] = f_sz;
    for c in 1..num_chunks {
        let target_doc = (c * CHUNK_DOCS) as u32;
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
            if p >= f_sz || id >= target_doc {
                c_byte[c] = p;
                hi = (lo + hi) / 2;
            } else {
                lo = p + 1;
            }
        }
    }

    let num_queries = queries.len();
    let mut thread_accs: Vec<Vec<QueryAcc>> = (0..NUM_T)
        .map(|_| vec![QueryAcc::default(); num_queries])
        .collect();
    let mut thread_arenas: Vec<Vec<u32>> = (0..NUM_T)
        .map(|_| Vec::with_capacity(32768))
        .collect();
    let accs_ptr = thread_accs.as_mut_ptr() as usize;
    let arenas_ptr = thread_arenas.as_mut_ptr() as usize;

    let next_chunk = AtomicUsize::new(0);

    std::thread::scope(|s| {
        for tid in 0..NUM_T {
            let next_c_ref = &next_chunk;
            let r_ref = &rep_map;
            let cb_ref = &c_byte;
            let queries_ref = &queries;
            let q_terms_ref = &q_terms;
            s.spawn(move || {
                let aff_mask = 1u64 << (tid % 12);
                sched_setaffinity(0, 8, &aff_mask);
                let accs = &mut *(accs_ptr as *mut Vec<QueryAcc>).add(tid);
                let top_arena = &mut *(arenas_ptr as *mut Vec<u32>).add(tid);

                let fw = alloc_mmap::<u16>(FW_CAP);
                let dp = alloc_mmap::<u32>(DP_CAP);
                let chunk_postings = alloc_mmap::<u16>(FW_CAP);
                let mut term_counts = vec![0u16; MAX_TERMS];
                let mut cur_pos = vec![0u32; MAX_TERMS];
                let mut touched_terms = Vec::with_capacity(16384);
                let mut chunk_bs_idx = vec![u16::MAX; MAX_TERMS];
                let mut chunk_bs = vec![0u64; 128 * 64];
                let mut bs_terms = Vec::with_capacity(128);
                let mut doc_ts = vec![0u32; CHUNK_DOCS];
                let mut seen = [0u64; 1280];
                let mut hl = [0u32; 2048];
                let mut fc = [[FastEntry { w0: 0, tid: u32::MAX, len: 0 }; 2]; 1024];
                let mut cand_uninit = MaybeUninit::<[u16; CHUNK_DOCS]>::uninit();
                let cand = cand_uninit.assume_init_mut();
                let mut curr_uninit = MaybeUninit::<[u64; 64]>::uninit();
                let curr = curr_uninit.assume_init_mut();

                loop {
                    let c = next_c_ref.fetch_add(1, Ordering::Relaxed);
                    if c >= num_chunks {
                        break;
                    }
                    let doc_s = (c * CHUNK_DOCS) as u32;
                    let doc_e = (total_docs).min(((c + 1) * CHUNK_DOCS) as u32);
                    let cd = (doc_e - doc_s) as usize;
                    let (mut p, pe) = (cb_ref[c], cb_ref[c + 1]);
                    madvise(d_buf.as_ptr().add(p) as *mut u8, pe - p, 3);

                    let mut fw_idx = 0usize;
                    for ld in 0..cd {
                        while *d_buf.get_unchecked(p) != b'\t' {
                            p += 1;
                        }
                        p += 1;
                        *doc_ts.get_unchecked_mut(ld) = p as u32;
                        *dp.add(ld) = fw_idx as u32;
                        let (mut nh, mut ov) = (0usize, false);
                        scan_doc(d_buf, &mut p, q_buf, r_ref, &mut fc, |t| {
                            let (w, b) = (t >> 6, 1u64 << (t & 63));
                            let sw = *seen.get_unchecked(w);
                            if sw & b == 0 {
                                *seen.get_unchecked_mut(w) = sw | b;
                                if nh < 2048 {
                                    *hl.get_unchecked_mut(nh) = t as u32;
                                    nh += 1;
                                } else {
                                    ov = true;
                                }
                                if *term_counts.get_unchecked(t) == 0 {
                                    touched_terms.push(t as u32);
                                }
                                *term_counts.get_unchecked_mut(t) += 1;
                                if t < 65535 {
                                    *fw.add(fw_idx) = t as u16;
                                    fw_idx += 1;
                                } else {
                                    *fw.add(fw_idx) = 65535;
                                    *fw.add(fw_idx + 1) = (t - 65535) as u16;
                                    fw_idx += 2;
                                }
                            }
                        });
                        if ov {
                            seen.fill(0);
                        } else {
                            for i in 0..nh {
                                *seen.get_unchecked_mut((*hl.get_unchecked(i) >> 6) as usize) = 0;
                            }
                        }
                    }
                    *dp.add(cd) = fw_idx as u32;

                    let mut tot = 0u32;
                    for &t in &touched_terms {
                        let cnt = *term_counts.get_unchecked(t as usize) as u32;
                        *cur_pos.get_unchecked_mut(t as usize) = tot;
                        tot += cnt;
                    }

                    for ld in 0..cd {
                        let start = *dp.add(ld) as usize;
                        let end = *dp.add(ld + 1) as usize;
                        let mut i = start;
                        while i < end {
                            let mut t = *fw.add(i) as usize;
                            i += 1;
                            if t == 65535 {
                                t = 65535 + *fw.add(i) as usize;
                                i += 1;
                            }
                            let pos = *cur_pos.get_unchecked(t);
                            *cur_pos.get_unchecked_mut(t) = pos + 1;
                            *chunk_postings.add(pos as usize) = ld as u16;
                        }
                    }

                    let bs_th = (cd / 20).max(10) as u16;
                    let cw = (cd + 63) >> 6;
                    let mut num_bs = 0usize;
                    for &t in &touched_terms {
                        let cnt = *term_counts.get_unchecked(t as usize);
                        if cnt >= bs_th && num_bs < 128 {
                            *chunk_bs_idx.get_unchecked_mut(t as usize) = num_bs as u16;
                            bs_terms.push(t);
                            let bs_slice = &mut chunk_bs[num_bs * cw..(num_bs + 1) * cw];
                            bs_slice.fill(0);
                            let end = *cur_pos.get_unchecked(t as usize) as usize;
                            let postings = std::slice::from_raw_parts(chunk_postings.add(end - cnt as usize), cnt as usize);
                            for &ld in postings {
                                *bs_slice.get_unchecked_mut((ld as usize) >> 6) |= 1u64 << (ld & 63);
                            }
                            num_bs += 1;
                        }
                    }

                    for q_idx in 0..queries_ref.len() {
                        let q = queries_ref.get_unchecked(q_idx);
                        let acc = accs.get_unchecked_mut(q_idx);
                        let q_ts = &q_terms_ref[q.t_idx as usize..q.t_idx as usize + q.k as usize];
                        if q.ib == 0 {
                            exec_bool_chunk(
                                q, q_ts, doc_s, cd as u32, cw,
                                &cur_pos, &term_counts, &chunk_bs_idx, &chunk_bs,
                                chunk_postings, cand, curr, acc, top_arena,
                            );
                        } else {
                            exec_phrase_chunk(
                                q, q_terms_ref, r_ref, doc_s, cd as u32, cw,
                                &cur_pos, &term_counts, &chunk_bs_idx, &chunk_bs,
                                chunk_postings, &doc_ts, d_buf, q_buf,
                                cand, acc, top_arena,
                            );
                        }
                    }

                    for &t in &touched_terms {
                        *term_counts.get_unchecked_mut(t as usize) = 0;
                    }
                    touched_terms.clear();
                    for &t in &bs_terms {
                        *chunk_bs_idx.get_unchecked_mut(t as usize) = u16::MAX;
                    }
                    bs_terms.clear();
                }

                free_mmap(fw, FW_CAP);
                free_mmap(dp, DP_CAP);
                free_mmap(chunk_postings, FW_CAP);
            });
        }
    });

    eprintln!("[STAGE 1] chunk loop finished: {:?}", t0.elapsed());

    let _ = std::fs::create_dir_all(&o_dir);
    let mut f = File::create(format!("{}/results.txt", o_dir)).unwrap();
    let mut out_buf = Vec::with_capacity(1 << 20);
    let mut cand_top = [0u32; 120];

    for q_idx in 0..num_queries {
        let mut total_cnt = 0u64;
        let mut total_sum = 0u64;
        let mut num_cand = 0usize;
        for tid in 0..NUM_T {
            let a = *thread_accs[tid].get_unchecked(q_idx);
            total_cnt += a.cnt as u64;
            total_sum += a.sum;
            let len = (a.head_len & 0xFF) as usize;
            if len > 0 {
                let slot = (a.head_len >> 8) as usize;
                let arena = &thread_arenas[tid];
                let docs = &arena[slot * 10..slot * 10 + len];
                cand_top[num_cand..num_cand + len].copy_from_slice(docs);
                num_cand += len;
            }
        }
        cand_top[..num_cand].sort_unstable();
        let final_len = num_cand.min(10);
        let mut top10 = [0u32; 10];
        top10[..final_len].copy_from_slice(&cand_top[..final_len]);
        fmt_res(total_cnt, total_sum, &top10, final_len, &mut out_buf);
        if out_buf.len() >= (512 << 10) {
            f.write_all(&out_buf).unwrap();
            out_buf.clear();
        }
    }
    if !out_buf.is_empty() {
        f.write_all(&out_buf).unwrap();
    }
    eprintln!("[STAGE 1] merge & write: {:?}", t0.elapsed());
    malloc_trim(0);
}

fn main() {
    unsafe {
        run_all();
    }
}
