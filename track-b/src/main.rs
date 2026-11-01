#![allow(static_mut_refs)]

const BIT_SUM_8: [u16; 256] = {
    let mut table = [0u16; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut s = 0u16;
        let mut b = 0usize;
        while b < 8 {
            if (i & (1 << b)) != 0 { s += b as u16; }
            b += 1;
        }
        table[i] = s;
        i += 1;
    }
    table
};

#[inline(always)]
fn word_bit_sum(val: u64) -> u64 {
    let b0 = (val & 0xFF) as usize;
    let b1 = ((val >> 8) & 0xFF) as usize;
    let b2 = ((val >> 16) & 0xFF) as usize;
    let b3 = ((val >> 24) & 0xFF) as usize;
    let b4 = ((val >> 32) & 0xFF) as usize;
    let b5 = ((val >> 40) & 0xFF) as usize;
    let b6 = ((val >> 48) & 0xFF) as usize;
    let b7 = ((val >> 56) & 0xFF) as usize;

    (BIT_SUM_8[b0] as u64)
        + (BIT_SUM_8[b1] as u64 + ((b1.count_ones() as u64) << 3))
        + (BIT_SUM_8[b2] as u64 + ((b2.count_ones() as u64) << 4))
        + (BIT_SUM_8[b3] as u64 + ((b3.count_ones() as u64) * 24))
        + (BIT_SUM_8[b4] as u64 + ((b4.count_ones() as u64) << 5))
        + (BIT_SUM_8[b5] as u64 + ((b5.count_ones() as u64) * 40))
        + (BIT_SUM_8[b6] as u64 + ((b6.count_ones() as u64) * 48))
        + (BIT_SUM_8[b7] as u64 + ((b7.count_ones() as u64) * 56))
}
use std::{
arch::x86_64::*,
fs::File,
io::Write,
mem::MaybeUninit,
os::unix::io::IntoRawFd,
ptr,
sync::atomic::{AtomicUsize, Ordering}, };
extern "C" { fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8; fn munmap(a: *mut u8, l: usize) -> i32; fn close(fd: i32) -> i32; fn mallopt(p: i32, v: i32) -> i32; fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32; fn malloc_trim(pad: usize) -> i32; fn sched_setaffinity(pid: i32, cpusetsize: usize, mask: *const u64) -> i32; }
const H_SZ: usize = 131072;
const H_MSK: usize = H_SZ - 1;
const NUM_T: usize = 12;
const NUM_C: usize = 24;
const FW_CAP: usize = 700_000;
const DP_CAP: usize = 32_000;
#[derive(Clone, Copy, Default)]
#[repr(C, align(16))]
struct Slot { w0: u64, h_tag: u32, tid: u32 }
#[derive(Clone, Copy, Default)]
struct TInfo { w0: u64, w1: u64, off: u32, df: u32, len: u16, _pad: u16 }
#[derive(Clone, Copy)]
struct Query { ib: u8, k: u8, ops: u16, t_idx: u32 }

#[derive(Clone, Copy, Default)]
#[repr(C, align(16))]
struct FastEntry {
    w0: u64,
    tid: u32,
    bi: u16,
    len: u8,
    _pad: u8,
}
const FC_SZ: usize = 2048;
const FC_MSK: usize = FC_SZ - 1;
static mut FAST_CACHE: [FastEntry; 2048] = [FastEntry { w0: 0, tid: u32::MAX, bi: u16::MAX, len: 0, _pad: 0 }; 2048];

const CMAP: [u8; 256] = {
let mut m = [0u8; 256];
m[b'\n' as usize] = 1;
let mut i = 0u8;
loop {
if i >= b'0' && i <= b'9'  { m[i as usize] = i; }
if i >= b'a' && i <= b'z' {
m[i as usize] = i;
m[(i - 32) as usize] = i; }
if i == 255  { break; }
i += 1; }
m };
#[repr(align(4096))]
struct AlignedVTab([Slot; H_SZ]);
static mut VTAB: AlignedVTab = AlignedVTab([Slot { w0: 0, h_tag: 0, tid: 0 }; H_SZ]);
static mut TINFO: [TInfo; 81920] = [TInfo { w0: 0, w1: 0, off: 0, df: 0, len: 0, _pad: 0 }; 81920];
static mut N_TERMS: usize = 0;
const PF_WORDS: usize = 4096; const PF_WMASK: usize = PF_WORDS - 1;

static mut PF: [u64; 4096] = [0; 4096];
#[inline(always)]
fn mix64(w0: u64, w1: u64, len: usize) -> u64 {
    let h = (w0 ^ (w1.wrapping_mul(0x9e3779b97f4a7c15).wrapping_add(len as u64))).wrapping_mul(0xbf58476d1ce4e5b9);
    h ^ (h >> 31)
}
#[inline(always)]
unsafe fn lookup_swar(w0: u64, h: u64, len: usize) -> u32 {
let (h32, mut idx) = (((h ^ (h >> 32)) as u32) | 1, (h as usize) & H_MSK);
let h_tag = (h32 & 0x00FFFFFF) | ((len as u32) << 24);
loop {
    let s = VTAB.0.get_unchecked(idx);
    if s.h_tag == 0 { return u32::MAX; }
    if s.h_tag == h_tag && s.w0 == w0 { return s.tid; }
    idx = (idx + 1) & H_MSK;
} }
unsafe fn insert_term(s: &[u8], off: u32, _q_buf: &[u8]) -> u32 {
let (mut w0, mut w1, len) = (0u64, 0u64, s.len());
ptr::copy_nonoverlapping(s.as_ptr(), &mut w0 as *mut _ as *mut u8, len.min(8));
if len > 8  { ptr::copy_nonoverlapping(s.as_ptr().add(8), &mut w1 as *mut _ as *mut u8, (len - 8).min(8)); }
let h = mix64(w0, w1, len);
let tid = lookup_swar(w0, h, len);
if tid != u32::MAX  { return tid; }
let (h32, mut idx) = (((h ^ (h >> 32)) as u32) | 1, (h as usize) & H_MSK);
let h_tag = (h32 & 0x00FFFFFF) | ((len as u32) << 24);
while VTAB.0[idx].h_tag != 0  { idx = (idx + 1) & H_MSK; }
let nt = N_TERMS as u32;
N_TERMS += 1;
VTAB.0[idx] = Slot { w0, h_tag, tid: nt };
TINFO[nt as usize] = TInfo { w0, w1, off, df: 0, len: len as u16, _pad: 0 };
let widx = ((h ^ (h >> 16)) as usize) & PF_WMASK;
let m = (1u64 << ((h >> 32) & 63)) | (1u64 << ((h >> 40) & 63));
PF[widx] |= m;
nt }
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
if m <= 1  { return (w0, 0, len); } }
let mut w1 = 0u64;
while len < 16 {
w1 |= (m as u64) << ((len - 8) * 8);
len += 1;
*p += 1;
m = *CMAP.get_unchecked(*d.get_unchecked(*p) as usize);
if m <= 1  { return (w0, w1, len); } }
while len < 32 {
*tail.get_unchecked_mut(len - 16) = m;
len += 1;
*p += 1;
m = *CMAP.get_unchecked(*d.get_unchecked(*p) as usize);
if m <= 1  { return (w0, w1, len); } }
while m > 1 {
*p += 1;
len += 1;
m = *CMAP.get_unchecked(*d.get_unchecked(*p) as usize); }
(w0, w1, len) }
#[inline(always)]
unsafe fn tokmask(c: __m256i) -> u32 {
let lo = _mm256_or_si256(c, _mm256_set1_epi8(0x20));
let t = _mm256_sub_epi8(lo, _mm256_set1_epi8(0x61));
let l = _mm256_cmpeq_epi8(_mm256_min_epu8(t, _mm256_set1_epi8(25)), t);
let d = _mm256_sub_epi8(c, _mm256_set1_epi8(0x30));
let n = _mm256_cmpeq_epi8(_mm256_min_epu8(d, _mm256_set1_epi8(9)), d);
_mm256_movemask_epi8(_mm256_or_si256(l, n)) as u32 }
#[target_feature(enable = "avx2,bmi1,bmi2,lzcnt,popcnt")]
#[inline]
unsafe fn scan_doc<F: FnMut(usize)>(d_buf: &[u8], p: &mut usize, _q_buf: &[u8], rep_map: &[u32], g_bs_ptr: *mut u64, w_per_doc: usize, d_w: usize, d_b: u64, mut fn_: F) {
let (base, n) = (d_buf.as_ptr(), d_buf.len());
let nlv = _mm256_set1_epi8(b'\n' as i8);
let (mut pos, mut p_tid, mut p_pos) = (0u32, u32::MAX, u32::MAX);
let mut tail = [0u8; 16];
let mut process_token = |w0: u64, w1: u64, len: usize, _tail: &[u8]| {
if len <= 8 {
    let fc_slot = ((w0 ^ (w0 >> 11)) as usize) & FC_MSK;
    let fc = *FAST_CACHE.get_unchecked(fc_slot);
    if fc.w0 == w0 && fc.len == len as u8 {
        let t = fc.tid as usize;
        if fc.bi != u16::MAX {
            *g_bs_ptr.add((fc.bi as usize) * w_per_doc + d_w) |= d_b;
        } else {
            fn_(t);
        }
        if p_tid == fc.tid && pos == p_pos + 1 && t < rep_map.len() {
            let rt = *rep_map.get_unchecked(t);
            if rt != u32::MAX { fn_(rt as usize); }
        }
        p_tid = fc.tid;
        p_pos = pos;
        pos += 1;
        return;
    }
}
let h = mix64(w0, w1, len);
let widx = ((h ^ (h >> 16)) as usize) & PF_WMASK;
let m = (1u64 << ((h >> 32) & 63)) | (1u64 << ((h >> 40) & 63));
if (*PF.get_unchecked(widx) & m) != m {
p_tid = u32::MAX;
pos += 1; return; }
let tid = lookup_swar(w0, h, len);
if tid != u32::MAX {
let t = tid as usize;
fn_(t);
if p_tid == tid && pos == p_pos + 1 && t < rep_map.len() {
let rt = *rep_map.get_unchecked(t);
if rt != u32::MAX  { fn_(rt as usize); } }
p_tid = tid;
p_pos = pos;
} else { p_tid = u32::MAX; }
pos += 1; };
while *p + 32 <= n {
let c = _mm256_loadu_si256(base.add(*p) as *const __m256i);
let mut mask_tok = tokmask(c);
let mask_nl = _mm256_movemask_epi8(_mm256_cmpeq_epi8(c, nlv)) as u32;
let nl_pos = if mask_nl != 0 { mask_nl.trailing_zeros() as usize } else { 32 };
if mask_nl != 0 { mask_tok &= (1u32 << nl_pos).wrapping_sub(1); }
let mut offset = 0usize;
let mut cross = false;
while mask_tok != 0 {
let zeros = mask_tok.trailing_zeros() as usize;
let tok_start = offset + zeros;
let rem = mask_tok >> zeros;
let len = (!rem).trailing_zeros() as usize;
if mask_nl == 0 && tok_start + len == 32 {
*p += tok_start;
cross = true; break; }
let (w0, w1);
if len <= 8 {
let raw = std::ptr::read_unaligned(base.add(*p + tok_start) as *const u64) | 0x2020202020202020;
w0 = core::arch::x86_64::_bzhi_u64(raw, (len << 3) as u32);
w1 = 0;
process_token(w0, w1, len, &[]);
} else if len <= 16 {
w0 = std::ptr::read_unaligned(base.add(*p + tok_start) as *const u64) | 0x2020202020202020;
let raw1 = std::ptr::read_unaligned(base.add(*p + tok_start + 8) as *const u64) | 0x2020202020202020;
w1 = core::arch::x86_64::_bzhi_u64(raw1, ((len - 8) << 3) as u32);
process_token(w0, w1, len, &[]);
} else {
let mut tp = *p + tok_start;
let r = read_tok(d_buf, &mut tp, &mut tail);
w0 = r.0;
w1 = r.1;
process_token(w0, w1, len, &tail); }
let consumed = zeros + len;
if consumed >= 32  { break; }
mask_tok = rem >> len;
offset = tok_start + len; }
if mask_nl != 0 { *p += nl_pos + 1; return; }
if cross {
let l = (!tokmask(_mm256_loadu_si256(base.add(*p) as *const __m256i))).trailing_zeros() as usize;
if l >= 16 || *p + 32 > n {
let r = read_tok(d_buf, p, &mut tail);
process_token(r.0, r.1, r.2, &tail); }
} else { *p += 32; } }
// Tail fallback
while *p < n {
while *p < n && *CMAP.get_unchecked(*base.add(*p) as usize) == 0  { *p += 1; }
if *p >= n || *base.add(*p) == b'\n' {
*p += 1; break; }
let r = read_tok(d_buf, p, &mut tail);
process_token(r.0, r.1, r.2, &tail); } }
#[inline(always)]
fn push_u64(buf: &mut Vec<u8>, mut n: u64) {
let (mut tmp, mut i) = ([0u8; 20], 20);
loop { i -= 1;
tmp[i] = b'0' + (n % 10) as u8;
n /= 10;
if n == 0  { break; } }
buf.extend_from_slice(&tmp[i..]); }
#[inline(always)]
fn fmt_res(cnt: u64, sum: u64, top: &[u32; 10], tc: usize, out: &mut Vec<u8>) {
if cnt == 0  { out.extend_from_slice(b"0 0\n"); } else {
push_u64(out, cnt);
out.push(b' ');
push_u64(out, sum);
for &d in &top[..tc] {
out.push(b' ');
push_u64(out, d as u64); }
out.push(b'\n'); } }
unsafe fn map_file(path: &str) -> &'static [u8] {
let f = File::open(path).unwrap();
let len = f.metadata().unwrap().len() as usize;
let fd = f.into_raw_fd();
let p = mmap(ptr::null_mut(), len, 1, 1, fd, 0);
close(fd);
madvise(p, len, 2);
std::slice::from_raw_parts(p, len) }
unsafe fn alloc_mmap<T>(count: usize) -> *mut T {
let sz = count * size_of::<T>();
let p = mmap(ptr::null_mut(), sz, 3, 0x22, -1, 0);
p as *mut T }
unsafe fn free_mmap<T>(p: *mut T, count: usize) {
munmap(p as *mut u8, count * size_of::<T>()); }
struct Engine<'a> {
c_offs: &'a [u32],
c_postings: [*const u16; NUM_C],
bs_idx: &'a [u16],
doc_s: &'a [u32; NUM_C],
doc_e: &'a [u32; NUM_C],
w_per_doc: usize,
g_bs: &'a [u64],
rep_map: &'a [u32],
doc_ts: &'a [u32],
d_buf: &'a [u8],
q_buf: &'a [u8],
q_terms: &'a [u32],
nt: usize, }
impl<'a> Engine<'a> {
#[inline(always)]
unsafe fn chunk_postings(&self, c: usize, tid: usize) -> &[u16] {
let base = c * self.nt;
let s = if tid > 0 { *self.c_offs.get_unchecked(base + tid - 1) } else { 0 } as usize;
let e = *self.c_offs.get_unchecked(base + tid) as usize;
std::slice::from_raw_parts(self.c_postings[c].add(s), e - s) }
#[inline(always)]
unsafe fn is_bs(&self, tid: usize) -> bool {
*self.bs_idx.get_unchecked(tid) != u16::MAX }
#[inline(always)]
unsafe fn intersect_and(&self, c: usize, t: usize, cs: u32, cand: &mut [u16], clen: usize) -> usize {
let mut w = 0usize;
if self.is_bs(t) {
let src = &self.g_bs[(self.bs_idx[t] as usize) * self.w_per_doc + ((cs >> 6) as usize)..];
for i in 0..clen {
let ld = cand[i] as usize;
cand[w] = ld as u16;
w += ((*src.get_unchecked(ld >> 6) >> (ld & 63)) & 1) as usize; }
} else {
let o_post = self.chunk_postings(c, t);
let olen = o_post.len();
if olen == 0 { return 0; }
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
j += 1; } } }
} else {
let (mut i, mut j) = (0usize, 0usize);
while i < clen && j < olen {
let (v1, v2) = (cand[i], *o_post.get_unchecked(j));
if v1 == v2 {
cand[w] = v1;
w += 1;
i += 1;
j += 1;
} else if v1 < v2  { i += 1; } else { j += 1; } } } }
w }
#[inline(always)]
unsafe fn intersect_not(&self, c: usize, t: usize, cs: u32, cand: &mut [u16], clen: usize) -> usize {
let mut w = 0usize;
if self.is_bs(t) {
let src = &self.g_bs[(self.bs_idx[t] as usize) * self.w_per_doc + ((cs >> 6) as usize)..];
for i in 0..clen {
let ld = cand[i] as usize;
cand[w] = ld as u16;
w += (((*src.get_unchecked(ld >> 6) >> (ld & 63)) & 1) ^ 1) as usize; }
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
} else { j += 1; } }
while i < clen {
cand[w] = cand[i];
w += 1;
i += 1; } }
w }
#[target_feature(enable = "avx2,bmi1,bmi2,lzcnt,popcnt")]
unsafe fn emit_bits(&self, curr: &[u64], cs: u32, cd: u32, cw: usize, t_cnt: &mut u64, t_sum: &mut u64, top: &mut [u32; 10], tc: &mut usize) {
let mut w = 0usize;
let v_zero = _mm256_setzero_si256();
while w < cw {
if w + 4 <= cw {
let v = _mm256_loadu_si256(curr.as_ptr().add(w) as *const __m256i);
let eq = _mm256_cmpeq_epi64(v, v_zero);
if (_mm256_movemask_epi8(eq) as u32) == 0xFFFFFFFF {
w += 4;
continue; } }
let mut val = *curr.get_unchecked(w);
if w == (cd >> 6) as usize {
let rem = cd & 63;
if rem != 0 { val &= (1u64 << rem) - 1; } }
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
*tc += 1; }
val &= val - 1; }
} else {
let cnt = val.count_ones() as u64;
*t_cnt += cnt;
*t_sum += cnt * (base_id as u64) + word_bit_sum(val); } }
w += 1; } }
#[target_feature(enable = "avx2,bmi1,bmi2,lzcnt,popcnt")]
unsafe fn exec_bool(&self, q: &Query, out: &mut Vec<u8>) {
let (mut t_cnt, mut t_sum, mut top, mut tc) = (0u64, 0u64, [0u32; 10], 0usize);
let q_ts = &self.q_terms[q.t_idx as usize..q.t_idx as usize + q.k as usize];
let mut curr_uninit = MaybeUninit::<[u64; 352]>::uninit();
let curr = curr_uninit.assume_init_mut();
let mut clist_uninit = MaybeUninit::<[u16; 21000]>::uninit();
let clist = clist_uninit.assume_init_mut();

let is_pure_and = (1..q.k as usize).all(|i| ((q.ops >> ((i - 1) * 2)) & 3) == 1);
if is_pure_and {
    for &tid in q_ts {
        if TINFO[tid as usize].df == 0 {
            return fmt_res(0, 0, &[0; 10], 0, out);
        }
    }
}
let mut sorted_and_terms = [0u32; 16];
let mut n_and_terms = 0usize;
let mut min_sparse_t = usize::MAX;
if is_pure_and {
    for &tid in q_ts {
        sorted_and_terms[n_and_terms] = tid;
        n_and_terms += 1;
    }
    sorted_and_terms[..n_and_terms].sort_unstable_by_key(|&tid| TINFO[tid as usize].df);
    for &tid in &sorted_and_terms[..n_and_terms] {
        if !self.is_bs(tid as usize) {
            min_sparse_t = tid as usize;
            break;
        }
    }
}
for c in 0..NUM_C {
let (cs, cd) = (self.doc_s[c], self.doc_e[c] - self.doc_s[c]);
if cd == 0  { continue; }
let cw = (cd as usize + 63) >> 6;
let (mut in_bs, mut clen) = (false, 0usize);
if is_pure_and && min_sparse_t != usize::MAX {
    let p0 = self.chunk_postings(c, min_sparse_t);
    clen = p0.len();
    if clen == 0 { continue; }
    clist[..clen].copy_from_slice(p0);
    for &tid in &sorted_and_terms[..n_and_terms] {
        if tid as usize != min_sparse_t {
            clen = self.intersect_and(c, tid as usize, cs, clist, clen);
            if clen == 0 { break; }
        }
    }
    t_cnt += clen as u64;
    let need = (10 - tc).min(clen);
    for i in 0..need { top[tc + i] = cs + clist[i] as u32; }
    tc += need;
    let mut s = 0u64;
    for &ld in &clist[..clen] { s += ld as u64; }
    t_sum += (cs as u64) * (clen as u64) + s;
    continue;
}
let t0 = q_ts[0] as usize;
if self.is_bs(t0) {
in_bs = true;
let src = &self.g_bs[(self.bs_idx[t0] as usize) * self.w_per_doc + ((cs >> 6) as usize)..];
curr[..cw].copy_from_slice(&src[..cw]);
} else {
let p = self.chunk_postings(c, t0);
clen = p.len();
clist[..clen].copy_from_slice(p); }
for i in 0..(q.k - 1) as usize {
let op = ((q.ops >> (i * 2)) & 3) as u8;
let nt_id = q_ts[i + 1] as usize;
if !in_bs && op == 2 {
curr[..cw].fill(0);
for &ld in &clist[..clen]  { *curr.get_unchecked_mut((ld as usize) >> 6) |= 1u64 << (ld & 63); }
in_bs = true; }
if in_bs {
if self.is_bs(nt_id) {
let src = &self.g_bs[(self.bs_idx[nt_id] as usize) * self.w_per_doc + ((cs >> 6) as usize)..];
let mut w = 0usize;
while w + 4 <= cw {
let (c_v, s_v) = (_mm256_loadu_si256(curr.as_ptr().add(w) as *const __m256i), _mm256_loadu_si256(src.as_ptr().add(w) as *const __m256i));
let res = match op {
1 => _mm256_and_si256(c_v, s_v),
2 => _mm256_or_si256(c_v, s_v),
_ => _mm256_andnot_si256(s_v, c_v), };
_mm256_storeu_si256(curr.as_mut_ptr().add(w) as *mut __m256i, res);
w += 4; }
while w < cw {
let s = *src.get_unchecked(w);
match op {
1 => *curr.get_unchecked_mut(w) &= s,
2 => *curr.get_unchecked_mut(w) |= s,
_ => *curr.get_unchecked_mut(w) &= !s, }
w += 1; }
} else if op == 1 {
let mut w = 0usize;
for &ld in self.chunk_postings(c, nt_id) {
if (*curr.get_unchecked((ld as usize) >> 6) & (1u64 << (ld & 63))) != 0 {
clist[w] = ld;
w += 1; } }
clen = w;
in_bs = false;
} else if op == 2 {
for &ld in self.chunk_postings(c, nt_id)  { *curr.get_unchecked_mut((ld as usize) >> 6) |= 1u64 << (ld & 63); }
} else {
for &ld in self.chunk_postings(c, nt_id)  { *curr.get_unchecked_mut((ld as usize) >> 6) &= !(1u64 << (ld & 63)); } }
} else if op == 1  { clen = self.intersect_and(c, nt_id, cs, clist, clen); } else if op == 3  { clen = self.intersect_not(c, nt_id, cs, clist, clen); } }
if !in_bs {
t_cnt += clen as u64;
let need = (10 - tc).min(clen);
for i in 0..need { top[tc + i] = cs + clist[i] as u32; }
tc += need;
let mut s = 0u64;
for &ld in &clist[..clen] { s += ld as u64; }
t_sum += (cs as u64) * (clen as u64) + s;
} else { self.emit_bits(curr, cs, cd, cw, &mut t_cnt, &mut t_sum, &mut top, &mut tc); } }
fmt_res(t_cnt, t_sum, &top, tc, out); }
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
nd += 1; }
true } };
for i in 0..k {
let t = q_ts[i] as usize;
if !add_dist(t) {
return fmt_res(0, 0, &[0; 10], 0, out); }
if i + 1 < k && t == q_ts[i + 1] as usize && t < self.rep_map.len() {
let rt = *self.rep_map.get_unchecked(t) as usize;
if rt != u32::MAX as usize && !add_dist(rt) {
return fmt_res(0, 0, &[0; 10], 0, out); }
} }
dist[..nd].sort_unstable_by_key(|&t| TINFO[t].df);
let (mut pw0, mut pw1, mut plen, mut poff) = ([0u64; 8], [0u64; 8], [0usize; 8], [0usize; 8]);
for i in 0..k {
let ti = *TINFO.get_unchecked(q_ts[i] as usize);
pw0[i] = ti.w0;
pw1[i] = ti.w1;
plen[i] = ti.len as usize;
poff[i] = ti.off as usize; }
let anchor = (0..k).min_by_key(|&i| TINFO[q_ts[i] as usize].df).unwrap();
let first_ch = (pw0[anchor] & 0xFF) as u8;
let first_up = if first_ch >= b'a' && first_ch <= b'z' { first_ch - 32 } else { first_ch };
let (v_lo, v_up, v_nl) = (_mm256_set1_epi8(first_ch as i8), _mm256_set1_epi8(first_up as i8), _mm256_set1_epi8(b'\n' as i8));
let c1_lo = ((pw0[anchor] >> 8) & 0xFF) as u8;
let c1_up = if c1_lo >= b'a' && c1_lo <= b'z' { c1_lo - 32 } else { c1_lo };
let (v1_lo, v1_up) = (_mm256_set1_epi8(c1_lo as i8), _mm256_set1_epi8(c1_up as i8));
let c2_lo = ((pw0[anchor] >> 16) & 0xFF) as u8;
let c2_up = if c2_lo >= b'a' && c2_lo <= b'z' { c2_lo - 32 } else { c2_lo };
let (v2_lo, v2_up) = (_mm256_set1_epi8(c2_lo as i8), _mm256_set1_epi8(c2_up as i8));
let p0_has_2 = plen[anchor] >= 2;
let p0_has_3 = plen[anchor] >= 3;
let (mut t_cnt, mut t_sum, mut top, mut tc) = (0u64, 0u64, [0u32; 10], 0usize);
let mut cand_uninit = MaybeUninit::<[u16; 21000]>::uninit();
let cand = cand_uninit.assume_init_mut();
let record_doc = |d: u32, t_cnt: &mut u64, t_sum: &mut u64, top: &mut [u32; 10], tc: &mut usize| {
*t_cnt += 1;
*t_sum += d as u64;
if *tc < 10 {
top[*tc] = d;
*tc += 1; } };
for c in 0..NUM_C {
let (cs, cd) = (self.doc_s[c], self.doc_e[c] - self.doc_s[c]);
if cd == 0  { continue; }
let (rare, mut clen) = (dist[0], 0usize);
if !self.is_bs(rare) {
let p = self.chunk_postings(c, rare);
clen = p.len();
if clen == 0  { continue; }
cand[..clen].copy_from_slice(p);
} else {
let cw = (cd as usize + 63) >> 6;
let src = &self.g_bs[(self.bs_idx[rare] as usize) * self.w_per_doc + ((cs >> 6) as usize)..];
for w in 0..cw {
let mut v = *src.get_unchecked(w);
while v != 0 {
let id = (w << 6) + v.trailing_zeros() as usize;
if id < cd as usize {
cand[clen] = id as u16;
clen += 1; }
v &= v - 1; } }
if clen == 0  { continue; } }
for &ot in &dist[1..nd] {
clen = self.intersect_and(c, ot, cs, cand, clen);
if clen == 0  { break; } }
if k == 1 {
    for ci in 0..clen {
        let d = cs + cand[ci] as u32;
        record_doc(d, &mut t_cnt, &mut t_sum, &mut top, &mut tc);
    }
    continue;
}
let (d_ptr, d_len) = (self.d_buf.as_ptr(), self.d_buf.len());
let is_alnum = |b: u8| -> bool { b.wrapping_sub(b'0') <= 9 || (b | 0x20).wrapping_sub(b'a') <= 25 };
let match_word = |tp: usize, j: usize| -> bool {
    let l = plen[j];
    if tp + l > d_len { return false; }
    if l <= 8 {
        let raw = ptr::read_unaligned(d_ptr.add(tp) as *const u64) | 0x2020202020202020;
        let mask = if l == 8 { u64::MAX } else { (1u64 << (l * 8)) - 1 };
        (raw & mask) == pw0[j]
    } else if l <= 16 {
        let raw0 = ptr::read_unaligned(d_ptr.add(tp) as *const u64) | 0x2020202020202020;
        let raw1 = ptr::read_unaligned(d_ptr.add(tp + 8) as *const u64) | 0x2020202020202020;
        let mask1 = if l == 16 { u64::MAX } else { (1u64 << ((l - 8) * 8)) - 1 };
        raw0 == pw0[j] && (raw1 & mask1) == pw1[j]
    } else {
        let raw0 = ptr::read_unaligned(d_ptr.add(tp) as *const u64) | 0x2020202020202020;
        let raw1 = ptr::read_unaligned(d_ptr.add(tp + 8) as *const u64) | 0x2020202020202020;
        if raw0 != pw0[j] || raw1 != pw1[j] { return false; }
        let q_tail = &self.q_buf[poff[j] + 16..poff[j] + l];
        let d_tail = std::slice::from_raw_parts(d_ptr.add(tp + 16), l - 16);
        for t_idx in 0..l - 16 {
            if (d_tail[t_idx] | 0x20) != q_tail[t_idx] { return false; }
        }
        true
    }
};
let match_phrase = |cand_p: usize, doc_start: usize| -> bool {
    if is_alnum(*d_ptr.add(cand_p.wrapping_sub(1))) { return false; }
    let l_a = plen[anchor];
    if is_alnum(*d_ptr.add(cand_p + l_a)) { return false; }
    if !match_word(cand_p, anchor) { return false; }

    let mut p = cand_p;
    for j in (0..anchor).rev() {
        if p <= doc_start { return false; }
        let mut lp = p - 1;
        while lp > doc_start && !is_alnum(*d_ptr.add(lp)) {
            if *d_ptr.add(lp) == b'\n' { return false; }
            lp -= 1;
        }
        let lj = plen[j];
        if lp + 1 < doc_start + lj { return false; }
        let w_start = lp + 1 - lj;
        if w_start > doc_start && is_alnum(*d_ptr.add(w_start - 1)) { return false; }
        if !match_word(w_start, j) { return false; }
        p = w_start;
    }

    let mut tp = cand_p + l_a;
    for j in anchor + 1..k {
        if tp >= d_len { return false; }
        if *d_ptr.add(tp) == b' ' && is_alnum(*d_ptr.add(tp + 1)) {
            tp += 1;
        } else {
            while !is_alnum(*d_ptr.add(tp)) {
                if *d_ptr.add(tp) == b'\n' { return false; }
                tp += 1;
                if tp >= d_len { return false; }
            }
        }
        let lj = plen[j];
        if !match_word(tp, j) { return false; }
        tp += lj;
        if is_alnum(*d_ptr.add(tp)) { return false; }
    }
    true
};
for ci in 0..clen {
if ci + 1 < clen {
    let nd = cs + cand[ci + 1] as u32;
    if (nd as usize) < self.doc_ts.len() {
        let nstart = *self.doc_ts.get_unchecked(nd as usize) as usize;
        if nstart < self.d_buf.len() {
            _mm_prefetch(self.d_buf.as_ptr().add(nstart) as *const i8, _MM_HINT_T0);
        }
    }
}
let d = cs + cand[ci] as u32;
let doc_start = *self.doc_ts.get_unchecked(d as usize) as usize;
let mut sp = doc_start;
let mut matched = false;
'doc_scan: loop {
if sp + 32 <= d_len {
let chunk = _mm256_loadu_si256(d_ptr.add(sp) as *const __m256i);
let m_ch = _mm256_or_si256(_mm256_cmpeq_epi8(chunk, v_lo), _mm256_cmpeq_epi8(chunk, v_up));
let m_nl = _mm256_cmpeq_epi8(chunk, v_nl);
let (mut mask_ch, mask_nl) = (_mm256_movemask_epi8(m_ch) as u32, _mm256_movemask_epi8(m_nl) as u32);
if p0_has_2 {
    let m_c1 = _mm256_or_si256(_mm256_cmpeq_epi8(chunk, v1_lo), _mm256_cmpeq_epi8(chunk, v1_up));
    let mask_c1 = _mm256_movemask_epi8(m_c1) as u32;
    let next_b1 = if sp + 32 < d_len { *d_ptr.add(sp + 32) } else { 0 };
    let c1_boundary = (next_b1 == c1_lo || next_b1 == c1_up) as u32;
    mask_ch &= (mask_c1 >> 1) | (c1_boundary << 31);
    if p0_has_3 {
        let m_c2 = _mm256_or_si256(_mm256_cmpeq_epi8(chunk, v2_lo), _mm256_cmpeq_epi8(chunk, v2_up));
        let mask_c2 = _mm256_movemask_epi8(m_c2) as u32;
        let next_b2 = if sp + 33 < d_len { *d_ptr.add(sp + 33) } else { 0 };
        let c2_b30 = (next_b1 == c2_lo || next_b1 == c2_up) as u32;
        let c2_b31 = (next_b2 == c2_lo || next_b2 == c2_up) as u32;
        mask_ch &= (mask_c2 >> 2) | (c2_b30 << 30) | (c2_b31 << 31);
    }
}
if mask_ch == 0 && mask_nl == 0 {
sp += 32; continue; }
let nl_pos = if mask_nl != 0 { mask_nl.trailing_zeros() } else { 32 };
let mut valid_ch = mask_ch & ((1u64 << nl_pos) - 1) as u32;
while valid_ch != 0 {
let bit = valid_ch.trailing_zeros() as usize;
valid_ch &= valid_ch - 1;
let cand_p = sp + bit;
let is_ok = match_phrase(cand_p, doc_start);
if is_ok {
matched = true;
break 'doc_scan; } }
if nl_pos < 32  { break 'doc_scan; }
sp += 32;
} else {
while *CMAP.get_unchecked(*self.d_buf.get_unchecked(sp) as usize) == 0  { sp += 1; }
if *self.d_buf.get_unchecked(sp) == b'\n'  { break 'doc_scan; }
if match_phrase(sp, doc_start) {
matched = true;
break 'doc_scan; }
let mut tail = [0u8; 16];
read_tok(self.d_buf, &mut sp, &mut tail); } }
if matched  { record_doc(d, &mut t_cnt, &mut t_sum, &mut top, &mut tc); } } }
fmt_res(t_cnt, t_sum, &top, tc, out); } }
#[target_feature(enable = "avx2,bmi1,bmi2,lzcnt,popcnt")]
unsafe fn worker_scan_fw(
c: usize,
_nt: usize,
w_per_doc: usize,
co_ptr: *mut u16,
doc_ts_ptr: *mut u32,
g_bs_ptr: *mut u64,
bs_idx: *const u16,
d_buf: &[u8],
q_buf: &[u8],
rep_map: &[u32],
doc_s: &[u32; NUM_C],
c_byte: &[usize; NUM_C + 1],
fw_ptr: *mut u16,
dp_ptr: *mut u32,
) {
let mut seen = [0u64; 1280];
let mut hl = [0u32; 2048];
let (mut p, pe, mut d, mut ld) = (c_byte[c], c_byte[c + 1], doc_s[c], 0u16);
let mut fw_idx = 0usize;
while p < pe {
while *d_buf.get_unchecked(p) != b'\t'  { p += 1; }
p += 1;
*doc_ts_ptr.add(d as usize) = p as u32;
*dp_ptr.add(ld as usize) = fw_idx as u32;
let d_w = (d as usize) >> 6;
let d_b = 1u64 << (d & 63);
let (mut nh, mut ov) = (0usize, false);
scan_doc(d_buf, &mut p, q_buf, rep_map, g_bs_ptr, w_per_doc, d_w, d_b, |t| {
let bi = *bs_idx.add(t);
if bi != u16::MAX {
    *g_bs_ptr.add((bi as usize) * w_per_doc + d_w) |= d_b;
} else {
    let (w, b) = (t >> 6, 1u64 << (t & 63));
    let sw = *seen.get_unchecked(w);
    if sw & b == 0 {
        *seen.get_unchecked_mut(w) = sw | b;
        if nh < 2048 {
            *hl.get_unchecked_mut(nh) = t as u32;
            nh += 1;
        } else { ov = true; }
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
} });
if ov  { seen.fill(0); } else {
for i in 0..nh  { *seen.get_unchecked_mut((*hl.get_unchecked(i) >> 6) as usize) = 0; } }
d += 1;
ld += 1; }
*dp_ptr.add(ld as usize) = fw_idx as u32;
let page_start = c_byte[c] & !4095;
let page_end = (c_byte[c + 1] + 4095) & !4095;
if page_end > page_start {
    madvise(d_buf.as_ptr().add(page_start) as *mut u8, page_end - page_start, 4);
} }
#[target_feature(enable = "avx2,bmi1,bmi2,lzcnt,popcnt")]
unsafe fn worker_transpose(
c: usize,
_nt: usize,
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
i += 1; }
let pos = *cur_ptr.add(t);
*cur_ptr.add(t) = pos + 1;
*chunk_u16_ptr.add(pos as usize) = ld as u16; }
} }
unsafe fn run_all() {
mallopt(-8, 1);
mallopt(-1, 0);
mallopt(-3, 65536);
let mut args = std::env::args().skip(1);
let (mut d_dir, mut o_dir) = ("/data".into(), "/out".into());
while let Some(a) = args.next() {
if a == "--data"  { d_dir = args.next().unwrap(); } else if a == "--out"  { o_dir = args.next().unwrap(); } }
let q_buf = map_file(&format!("{}/queries.txt", d_dir));
let d_buf = map_file(&format!("{}/docs.txt", d_dir));
let mut queries = Vec::with_capacity(50000);
let mut q_terms = Vec::with_capacity(160000);
let (mut qp, qe) = (0usize, q_buf.len());
while qp < qe {
while qp < qe && q_buf[qp] <= b' '  { qp += 1; }
if qp >= qe  { break; }
let ib = q_buf[qp..].starts_with(b"BOOL ");
qp += if ib { 5 } else { 8 };
let mut le = qp;
while le < qe && q_buf[le] != if ib { b'\n' } else { b'"' } {
le += 1; }
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
q.k += 1; }
} }
queries.push(q);
qp = le;
while qp < qe && q_buf[qp] != b'\n'  { qp += 1; }
qp += 1; }
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
TINFO[rt as usize] = TINFO[t]; }
} } }
let f_sz = d_buf.len();
let mut pl = f_sz - 1;
while pl > 0 && d_buf[pl - 1] != b'\n'  { pl -= 1; }
let mut total_docs = 0u32;
while pl < f_sz && d_buf[pl] >= b'0' && d_buf[pl] <= b'9' {
total_docs = total_docs * 10 + (d_buf[pl] - b'0') as u32;
pl += 1; }
total_docs += 1;
let nt = N_TERMS;
let w_per_doc = ((total_docs + 63) >> 6) as usize;
let mut bs_idx = vec![u16::MAX; nt];
let mut n_bs = 0usize;
// Pre-sample 600 docs to detect heavy terms (>5.6% df) up-front
let sample_docs = total_docs.min(600);
let mut sample_counts = vec![0u16; nt];
let mut sp = 0usize;
let mut s_seen = [0u64; 1280];
let mut s_hl = [0u32; 2048];
for _ in 0..sample_docs {
while sp < f_sz && d_buf[sp] != b'\t'  { sp += 1; }
if sp >= f_sz  { break; }
sp += 1;
let (mut nh, mut ov) = (0usize, false);
scan_doc(d_buf, &mut sp, q_buf, &rep_map, std::ptr::null_mut(), 0, 0, 0, |t| {
let (w, b) = (t >> 6, 1u64 << (t & 63));
let sw = *s_seen.get_unchecked(w);
if sw & b == 0 {
*s_seen.get_unchecked_mut(w) = sw | b;
if nh < 2048 { *s_hl.get_unchecked_mut(nh) = t as u32; nh += 1; } else { ov = true; }
sample_counts[t] = sample_counts[t].saturating_add(1); } });
if ov { s_seen.fill(0); } else { for i in 0..nh { *s_seen.get_unchecked_mut((*s_hl.get_unchecked(i) >> 6) as usize) = 0; } } }
// Terms with >= 34 hits in 600 docs (~5.6%) are designated bitset terms
let threshold = if total_docs >= 25000 { 34 } else { 15 };
for i in 0..nt {
if sample_counts[i] >= threshold {
bs_idx[i] = n_bs as u16;
n_bs += 1; } }
let mut term_order: Vec<u32> = (0..nt as u32).collect();
term_order.sort_unstable_by_key(|&t| std::cmp::Reverse(sample_counts[t as usize]));
for &t in &term_order {
    let ti = &TINFO[t as usize];
    if ti.len <= 8 && sample_counts[t as usize] > 0 {
        let w0 = ti.w0;
        let slot = ((w0 ^ (w0 >> 11)) as usize) & FC_MSK;
        if FAST_CACHE[slot].w0 == 0 {
            FAST_CACHE[slot] = FastEntry {
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
doc_s[c] = total_docs.min(if c > 0 { (c as u32 * dpc + 63) & !63 } else { 0 });
if c > 0  { doc_e[c - 1] = doc_s[c]; } }
doc_e[NUM_C - 1] = total_docs;
for c in 1..NUM_C {
let (mut lo, mut hi) = (0, f_sz);
while lo < hi {
let mut p = (lo + hi) / 2;
while p > 0 && d_buf[p - 1] != b'\n'  { p += 1; }
let (mut id, mut cp) = (0u32, p);
while cp < f_sz && d_buf[cp] >= b'0' && d_buf[cp] <= b'9' {
id = id * 10 + (d_buf[cp] - b'0') as u32;
cp += 1; }
if p >= f_sz || id >= doc_s[c] {
c_byte[c] = p;
hi = (lo + hi) / 2;
} else { lo = p + 1; } } }
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
dp_ptrs[t] = alloc_mmap::<u32>(DP_CAP) as usize; }
let next_chunk = AtomicUsize::new(0);
let barrier1 = std::sync::Barrier::new(NUM_T);
let barrier2 = std::sync::Barrier::new(NUM_T);
std::thread::scope(|s| {
for tid in 1..NUM_T {
let b1_ref = &barrier1;
let b2_ref = &barrier2;
let next_c_ref = &next_chunk;
let r_ref = &rep_map;
let ds_ref = &doc_s;
let de_ref = &doc_e;
let cb_ref = &c_byte;
let queries_ref = &queries;
let q_terms_ref = &q_terms;
let next_ref = &next_b;
let c_post_ptrs_ref = &c_post_ptrs;
let c_post_lens_ref = &c_post_lens;
let fw_ptr_c = fw_ptrs[tid];
let dp_ptr_c = dp_ptrs[tid];
std::thread::Builder::new()
.stack_size(196608)
.spawn_scoped(s, move || {
let aff_mask = 1u64 << (tid % 12);
sched_setaffinity(0, 8, &aff_mask);
let fw = fw_ptr_c as *mut u16;
let dp = dp_ptr_c as *mut u32;
let thread_cnt_p = (thread_counts_val as *mut u16).add(tid * nt);
let bs_idx_p = bs_idx_ptr as *const u16;
loop { let c = next_c_ref.fetch_add(1, Ordering::Relaxed);
if c >= NUM_C  { break; }
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
q_buf,
r_ref,
ds_ref,
cb_ref,
fw,
dp,
);
let c_off_p = (c_offs_val as *mut u32).add(c * nt);
let mut my_tot = 0u32;
for i in 0..nt {
*c_off_p.add(i) = my_tot;
if *bs_idx_p.add(i) == u16::MAX  { my_tot += *thread_cnt_p.add(i) as u32; } }
let my_total = (my_tot as usize).max(1);
let chunk_u16 = alloc_mmap::<u16>(my_total);
c_post_ptrs_ref[c].store(chunk_u16 as usize, Ordering::Release);
c_post_lens_ref[c].store(my_total, Ordering::Relaxed);
worker_transpose(
c,
nt,
c_off_p,
chunk_u16,
ds_ref,
de_ref,
fw,
dp,
); }
free_mmap(fw, FW_CAP);
free_mmap(dp, DP_CAP);
b1_ref.wait();
let chunk_nt = (nt + NUM_T - 1) / NUM_T;
let t_start = tid * chunk_nt;
let t_end = (t_start + chunk_nt).min(nt);
let my_len = t_end.saturating_sub(t_start);
let mut local_df = vec![0u32; my_len];
for c in 0..NUM_C {
    let base = c * nt;
    let slice = std::slice::from_raw_parts((c_offs_val as *const u32).add(base), nt);
    for idx in 0..my_len {
        let i = t_start + idx;
        let end = slice[i];
        let start = if i > 0 { slice[i - 1] } else { 0 };
        local_df[idx] += end - start;
    }
}
for idx in 0..my_len {
    let i = t_start + idx;
    let bi = *bs_idx_p.add(i);
    if bi != u16::MAX {
        let mut df = 0u32;
        let src = std::slice::from_raw_parts((raw_bs_val as *const u64).add((bi as usize) * w_per_doc), w_per_doc);
        for &w in src { df += w.count_ones(); }
        TINFO[i].df = df;
    } else {
        TINFO[i].df = local_df[idx];
    }
}
b2_ref.wait();
let mut cp_ptrs = [ptr::null(); NUM_C];
for c in 0..NUM_C  { cp_ptrs[c] = c_post_ptrs_ref[c].load(Ordering::Relaxed) as *const u16; }
let bi_slice = std::slice::from_raw_parts(bs_idx_ptr as *const u16, nt);
let c_off_slice = std::slice::from_raw_parts(c_offs_val as *const u32, NUM_C * nt);
let g_bs_slice = std::slice::from_raw_parts(raw_bs_val as *const u64, g_bs_total);
let doc_ts_slice = std::slice::from_raw_parts(doc_ts_ptr as *const u32, total_docs as usize);
let eng = Engine {
c_offs: c_off_slice,
c_postings: cp_ptrs,
bs_idx: bi_slice,
doc_s: ds_ref,
doc_e: de_ref,
w_per_doc,
g_bs: g_bs_slice,
rep_map: r_ref,
doc_ts: doc_ts_slice,
d_buf,
q_buf,
q_terms: q_terms_ref,
nt, };
loop { let b = next_ref.fetch_add(1, Ordering::Relaxed);
if b >= nb  { break; }
let ob = &mut *(out_bufs_ptr as *mut Vec<u8>).add(b);
for q in &queries_ref[b * QB..((b + 1) * QB).min(queries_ref.len())] {
if q.ib == 0  {
    eng.exec_bool(q, ob);
} else {
    eng.exec_phrase(q, ob);
} } }
})
.unwrap(); }
let aff_mask0 = 1u64;
sched_setaffinity(0, 8, &aff_mask0);
let fw0 = fw_ptrs[0] as *mut u16;
let dp0 = dp_ptrs[0] as *mut u32;
let thread_cnt_0 = thread_counts as *mut u16;
let bs_idx_p = bs_idx.as_ptr();
loop { let c = next_chunk.fetch_add(1, Ordering::Relaxed);
if c >= NUM_C  { break; }
ptr::write_bytes(thread_cnt_0, 0, nt);
worker_scan_fw(
c,
nt,
w_per_doc,
thread_cnt_0,
doc_ts_ptr as *mut u32,
raw_bs_ptr,
bs_idx_p,
d_buf,
q_buf,
&rep_map,
&doc_s,
&c_byte,
fw0,
dp0,
);
let c_off_p = c_offs.add(c * nt);
let mut my_tot = 0u32;
for i in 0..nt {
*c_off_p.add(i) = my_tot;
if *bs_idx_p.add(i) == u16::MAX  { my_tot += *thread_cnt_0.add(i) as u32; } }
let my_total = (my_tot as usize).max(1);
let chunk_u16 = alloc_mmap::<u16>(my_total);
c_post_ptrs[c].store(chunk_u16 as usize, Ordering::Release);
c_post_lens[c].store(my_total, Ordering::Relaxed);
worker_transpose(
c,
nt,
c_off_p,
chunk_u16,
&doc_s,
&doc_e,
fw0,
dp0,
); }
free_mmap(fw0, FW_CAP);
free_mmap(dp0, DP_CAP);
barrier1.wait();
let chunk_nt = (nt + NUM_T - 1) / NUM_T;
let t_start = 0;
let t_end = chunk_nt.min(nt);
let my_len = t_end.saturating_sub(t_start);
let mut local_df = vec![0u32; my_len];
for c in 0..NUM_C {
    let base = c * nt;
    let slice = std::slice::from_raw_parts(c_offs.add(base), nt);
    for idx in 0..my_len {
        let i = t_start + idx;
        let end = slice[i];
        let start = if i > 0 { slice[i - 1] } else { 0 };
        local_df[idx] += end - start;
    }
}
for idx in 0..my_len {
    let i = t_start + idx;
    let bi = *bs_idx_p.add(i);
    if bi != u16::MAX {
        let mut df = 0u32;
        let src = std::slice::from_raw_parts(raw_bs_ptr.add((bi as usize) * w_per_doc), w_per_doc);
        for &w in src { df += w.count_ones(); }
        TINFO[i].df = df;
    } else {
        TINFO[i].df = local_df[idx];
    }
}
barrier2.wait();
madvise(VTAB.0.as_mut_ptr() as *mut u8, size_of::<AlignedVTab>(), 4);
free_mmap(thread_counts, NUM_T * nt);
let mut cp_ptrs = [ptr::null(); NUM_C];
for c in 0..NUM_C  { cp_ptrs[c] = c_post_ptrs[c].load(Ordering::Relaxed) as *const u16; }
let c_off_slice = std::slice::from_raw_parts(c_offs as *const u32, sc_count);
let g_bs_slice = std::slice::from_raw_parts(raw_bs_ptr as *const u64, g_bs_total);
let doc_ts_slice = std::slice::from_raw_parts(doc_ts.as_ptr(), total_docs as usize);
let eng = Engine {
c_offs: c_off_slice,
c_postings: cp_ptrs,
bs_idx: &bs_idx,
doc_s: &doc_s,
doc_e: &doc_e,
w_per_doc,
g_bs: g_bs_slice,
rep_map: &rep_map,
doc_ts: doc_ts_slice,
d_buf,
q_buf,
q_terms: &q_terms,
nt, };
loop { let b = next_b.fetch_add(1, Ordering::Relaxed);
if b >= nb  { break; }
let ob = &mut *(out_bufs_ptr as *mut Vec<u8>).add(b);
for q in &queries[b * QB..((b + 1) * QB).min(queries.len())] {
if q.ib == 0  { eng.exec_bool(q, ob); } else { eng.exec_phrase(q, ob); } } }
});
let _ = std::fs::create_dir_all(&o_dir);
let f = File::create(format!("{}/results.txt", o_dir)).unwrap();
let mut writer = std::io::BufWriter::with_capacity(262144, f);
for ob in &out_bufs { writer.write_all(ob).unwrap(); }
writer.flush().unwrap();
free_mmap(raw_bs_ptr, g_bs_total);
for c in 0..NUM_C  { free_mmap(c_post_ptrs[c].load(Ordering::Relaxed) as *mut u16, c_post_lens[c].load(Ordering::Relaxed)); }
free_mmap(c_offs, sc_count);
drop(doc_ts);
drop(queries);
drop(q_terms);
malloc_trim(0); }
fn main() {
unsafe { run_all();
} }
