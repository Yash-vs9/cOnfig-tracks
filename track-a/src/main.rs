// Speed Demon 2026 — Track A (Ingest) Ultra-Fast & Compact Championship Rust Solution
// Targets: T <= 0.022 s, M <= 24 MB, S <= 210 lines, D = 1.00 -> 83-86+ Points
#![allow(unsafe_op_in_unsafe_fn, static_mut_refs, dead_code)]
use std::{arch::x86_64::*, fs::File, os::unix::io::AsRawFd, ptr::copy_nonoverlapping as copy, slice::from_raw_parts as slc, sync::{atomic::{AtomicU32, AtomicU64, Ordering::*}, Barrier}};

extern "C" {
    fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
    fn write(fd: i32, b: *const u8, c: usize) -> isize;
}

#[repr(C, align(64))]
#[derive(Copy, Clone)]
struct TState { head: [i32; 80], cur: [i32; 80], fill: [u16; 80] }

const BLK_SZ: usize = 256;
const N_BLK: usize = 14336;

static mut HR_KV: [u64; 256] = [0; 256];
static mut EP_KV: [u64; 8192] = [0; 8192];
static mut G_ERR: [u32; 80 * 2048] = [0; 80 * 2048];
static N_CNT: [AtomicU32; 4] = [AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)];

static mut HR_PTR: [usize; 80] = [0; 80];
static mut EP_PTR: [usize; 2048] = [0; 2048];
static mut EP_LEN: [u8; 2048] = [0; 2048];
static mut ORD_H: [u8; 80] = [0; 80];
static mut ORD_E: [u16; 2048] = [0; 2048];
static mut BLK_FILL: [u16; N_BLK] = [0; N_BLK];
static mut BLK_NEXT: [i32; N_BLK] = [0; N_BLK];
static mut POOL: [u32; N_BLK * BLK_SZ] = [0; N_BLK * BLK_SZ];
static mut BIG_LAT: [u32; 262144] = [0; 262144];
static mut TS: [TState; 12] = [TState { head: [0; 80], cur: [0; 80], fill: [0; 80] }; 12];
static mut T_OUT_LEN: [usize; 12] = [0; 12];
static mut T_OUT: [[u8; 163840]; 12] = [[0; 163840]; 12];

#[inline(always)]
unsafe fn fmt_u32_sp(p: *mut u8, mut v: u32) -> *mut u8 {
    let (mut b, mut i) = ([b' '; 12], 11);
    loop {
        i -= 1;
        (b[i], v) = (b'0' + (v % 10) as u8, v / 10);
        if v == 0 { break; }
    }
    copy(b.as_ptr().add(i), p, 12 - i);
    p.add(12 - i)
}

#[cold]
unsafe fn intern(is_ep: bool, p: *const u8, len: u8, h: u64) -> u16 {
    let (tab, mask, ci) = if is_ep { (EP_KV.as_ptr(), 8191, 1) } else { (HR_KV.as_ptr(), 255, 0) };
    let mut s = ((h >> 16) as usize) & mask;
    loop {
        let cell = &*(tab.add(s) as *const AtomicU64);
        let kv = cell.load(Acquire);
        if (kv ^ h) < 65536 && kv != 0 { return (kv & 0xFFFF) as u16; }
        if kv == 0 && cell.compare_exchange(0, 1, AcqRel, Acquire).is_ok() {
            let id = N_CNT[ci].fetch_add(1, Relaxed) as usize;
            if is_ep { (EP_PTR[id], EP_LEN[id]) = (p as usize, len); } else { HR_PTR[id] = p as usize; }
            cell.store(h | (id as u64), Release);
            return id as u16;
        }
        while cell.load(Acquire) == 1 { _mm_pause(); }
        let kv2 = cell.load(Acquire);
        if (kv2 ^ h) < 65536 && kv2 != 0 { return (kv2 & 0xFFFF) as u16; }
        s = (s + 1) & mask;
    }
}

fn main() {
    unsafe {
        let mut a = std::env::args().skip(1);
        let in_p = a.next().unwrap_or_else(|| "/data/requests.log".into());
        let out_p = a.next().unwrap_or_else(|| "/out/summary.txt".into());
        let (Ok(inf), Ok(outf)) = (File::open(&in_p), File::create(&out_p)) else { return };
        let sz = inf.metadata().map(|m| m.len() as usize).unwrap_or(0);
        if sz == 0 { return; }
        let (fd, out_fd) = (inf.as_raw_fd(), outf.as_raw_fd());
        let mut base = mmap(std::ptr::null_mut(), sz, 1, 1, fd, 0);
        if base as isize == -1 { base = mmap(std::ptr::null_mut(), sz, 1, 2, fd, 0); }
        if base as isize == -1 { return; }
        let (bar, base_addr) = (Barrier::new(12), base as usize);

        std::thread::scope(|sc| {
            for tid in 0..12 {
                let bar = &bar;
                sc.spawn(move || {
                    let b_ptr = base_addr as *const u8;
                    let (mut s, mut e) = (if tid > 0 { (tid * sz) / 12 } else { 0 }, if tid < 11 { ((tid + 1) * sz) / 12 } else { sz });
                    if s > 0 { s += slc(b_ptr.add(s - 1), sz - s + 1).iter().position(|&c| c == b'\n').unwrap(); }
                    if e < sz { e += slc(b_ptr.add(e - 1), sz - e + 1).iter().position(|&c| c == b'\n').unwrap(); }
                    let my = &mut TS[tid];
                    *my = TState { head: [-1; 80], cur: [-1; 80], fill: [0; 80] };
                    let (mut last_hw, mut hid, mut tl_ep) = (0u64, 0usize, [0u64; 512]);
                    let (mut p, end_c, sp_v) = (b_ptr.add(s), b_ptr.add(e), _mm256_set1_epi8(b' ' as i8));

                    while p < end_c {
                        let hw = (p.add(5) as *const u64).read_unaligned();
                        if hw != last_hw {
                            last_hw = hw;
                            let hh = (((_mm_crc32_u64(0, (p as *const u64).read_unaligned()) << 32) ^ hw.wrapping_mul(0x9e3779b97f4a7c15)) & !0xFFFF) | (1 << 63);
                            hid = intern(false, p, 13, hh) as usize;
                        }
                        let mut ep = p.add(32);
                        while *ep != b'/' { ep = ep.add(1); }
                        let m = if ep.add(32) <= end_c { _mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256(ep as _), sp_v)) as u32 } else { 0 };
                        let elen = if m != 0 { m.trailing_zeros() as usize } else { slc(ep, (end_c as usize) - (ep as usize)).iter().position(|&c| c == b' ').unwrap() };

                        let (mut h1, mut h2, mut k) = (elen as u64, (elen as u64).wrapping_mul(0x9e3779b97f4a7c15), 0usize);
                        while k + 8 <= elen {
                            let w = (ep.add(k) as *const u64).read_unaligned();
                            (h1, h2, k) = (_mm_crc32_u64(h1, w), (h2 ^ w).wrapping_mul(0x9e3779b97f4a7c15), k + 8);
                        }
                        if k < elen {
                            let w = (ep.add(k) as *const u64).read_unaligned() & ((1u64 << ((elen - k) * 8)) - 1);
                            (h1, h2) = (_mm_crc32_u64(h1, w), (h2 ^ w).wrapping_mul(0x9e3779b97f4a7c15));
                        }
                        let h = (((h1 << 32) ^ h2) & !0xFFFF) | (1 << 63);
                        let (c_idx, c_kv) = (((h >> 16) as usize) & 511, tl_ep[((h >> 16) as usize) & 511]);
                        let eid = if (c_kv ^ h) < 65536 { (c_kv & 0xFFFF) as u16 } else {
                            let id = intern(true, ep, elen as u8, h);
                            tl_ep[c_idx] = h | (id as u64);
                            id
                        };
                        let st_p = ep.add(elen + 1);
                        if *st_p == b'5' { (*(G_ERR.as_ptr().add(hid * 2048 + (eid as usize)) as *const AtomicU32)).fetch_add(1, Relaxed); }
                        let (mut c, mut lat) = (st_p.add(5), (*st_p.add(4) - b'0') as u32);
                        while *c >= b'0' { (lat, c) = (lat * 10 + (*c - b'0') as u32, c.add(1)); }
                        p = c.add(if *c == b'\r' { 2 } else { 1 });

                        let (mut b, mut f) = (my.cur[hid], my.fill[hid] as usize);
                        if b < 0 || f == BLK_SZ {
                            if b >= 0 { BLK_FILL[b as usize] = BLK_SZ as u16; }
                            let nb = N_CNT[2].fetch_add(1, Relaxed) as i32;
                            BLK_NEXT[nb as usize] = -1;
                            if b < 0 { my.head[hid] = nb; } else { BLK_NEXT[b as usize] = nb; }
                            (my.cur[hid], b, f) = (nb, nb, 0);
                        }
                        let pk = if lat < (1 << 20) { (lat << 11) | (eid as u32) } else {
                            let bi = N_CNT[3].fetch_add(1, Relaxed);
                            BIG_LAT[bi as usize] = lat;
                            0x8000_0000 | (bi << 11) | (eid as u32)
                        };
                        (POOL[(b as usize) * BLK_SZ + f], my.fill[hid]) = (pk, (f + 1) as u16);
                    }
                    (0..80).filter(|&h| my.cur[h] >= 0).for_each(|h| BLK_FILL[my.cur[h] as usize] = my.fill[h]);
                    bar.wait();
                    if tid == 0 {
                        let (nh, ne) = (N_CNT[0].load(Relaxed) as usize, N_CNT[1].load(Relaxed) as usize);
                        (0..nh).for_each(|i| ORD_H[i] = i as u8);
                        (0..ne).for_each(|i| ORD_E[i] = i as u16);
                        ORD_H[..nh].sort_unstable_by(|&x, &y| slc(HR_PTR[x as usize] as _, 13).cmp(slc(HR_PTR[y as usize] as _, 13)));
                        ORD_E[..ne].sort_unstable_by(|&x, &y| slc(EP_PTR[x as usize] as _, EP_LEN[x as usize] as _).cmp(slc(EP_PTR[y as usize] as _, EP_LEN[y as usize] as _)));
                    }
                    bar.wait();
                    let (nh, ne) = (N_CNT[0].load(Relaxed) as usize, N_CNT[1].load(Relaxed) as usize);
                    let (mut cnt, mut off, mut lats, mut base_hi) = ([0u32; 2048], [0u32; 2048], Vec::<u32>::new(), 0);
                    while base_hi < nh {
                        let (hi, mut out_len) = (base_hi + tid, 0usize);
                        if hi < nh {
                            let (h_id, mut tot) = (ORD_H[hi] as usize, 0u32);
                            cnt[..ne].fill(0);
                            for t in 0..12 {
                                let mut b = TS[t].head[h_id];
                                while b >= 0 {
                                    (0..BLK_FILL[b as usize] as usize).for_each(|i| cnt[(POOL[(b as usize) * BLK_SZ + i] & 2047) as usize] += 1);
                                    b = BLK_NEXT[b as usize];
                                }
                            }
                            for i in 0..ne { (off[i], tot, cnt[i]) = (tot, tot + cnt[i], 0); }
                            lats.resize(tot as usize, 0);
                            for t in 0..12 {
                                let mut b = TS[t].head[h_id];
                                while b >= 0 {
                                    for i in 0..BLK_FILL[b as usize] as usize {
                                        let v = POOL[(b as usize) * BLK_SZ + i];
                                        let eid = (v & 2047) as usize;
                                        let lat = if (v as i32) >= 0 { v >> 11 } else { BIG_LAT[((v >> 11) & 0xF_FFFF) as usize] };
                                        lats[(off[eid] + cnt[eid]) as usize] = lat;
                                        cnt[eid] += 1;
                                    }
                                    b = BLK_NEXT[b as usize];
                                }
                            }
                            let out_b = T_OUT[tid].as_mut_ptr();
                            let mut ptr = out_b;
                            for ei in 0..ne {
                                let (eid, n) = (ORD_E[ei] as usize, cnt[ORD_E[ei] as usize] as usize);
                                if n == 0 { continue; }
                                let l = &mut lats[off[eid] as usize..off[eid] as usize + n];
                                let (i50, i95, i99) = ((50 * n + 99) / 100 - 1, (95 * n + 99) / 100 - 1, (99 * n + 99) / 100 - 1);
                                if n <= 32 { l.sort_unstable(); } else {
                                    l.select_nth_unstable(i50);
                                    if i95 > i50 { l[i50 + 1..].select_nth_unstable(i95 - i50 - 1); }
                                    if i99 > i95 { l[i95 + 1..].select_nth_unstable(i99 - i95 - 1); }
                                }
                                let err = G_ERR[h_id * 2048 + eid];
                                let el = EP_LEN[eid] as usize;
                                let ppm = ((err as u64) * 1_000_000 / (n as u64)) as u32;
                                copy(HR_PTR[h_id] as *const u8, ptr, 13);
                                copy(b":00:00Z ".as_ptr(), ptr.add(13), 8);
                                copy(EP_PTR[eid] as *const u8, ptr.add(21), el);
                                *ptr.add(21 + el) = b' ';
                                ptr = ptr.add(22 + el);
                                [n as u32, l[i50], l[i95], l[i99], err, ppm].into_iter().for_each(|v| ptr = fmt_u32_sp(ptr, v));
                                *ptr.sub(1) = b'\n';
                            }
                            out_len = ptr as usize - out_b as usize;
                        }
                        T_OUT_LEN[tid] = out_len;
                        bar.wait();
                        if tid == 0 {
                            for k in 0..12 {
                                if base_hi + k < nh && T_OUT_LEN[k] > 0 { write(out_fd, T_OUT[k].as_ptr(), T_OUT_LEN[k]); }
                            }
                        }
                        bar.wait();
                        base_hi += 12;
                    }
                });
            }
        });
    }
}
