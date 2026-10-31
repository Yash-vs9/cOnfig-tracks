// Speed Demon 2026 — Track A (Ingest) Championship Rust Solution
// Zero external dependencies (D = 1), 100% .bss zero-initialized static arena, offline build.
// 1. Zero-copy mmap(MAP_SHARED) over pre-warmed /data/requests.log (0 B cgroup charge).
// 2. Cache-line aligned ThreadState (#[repr(C, align(64))]) eliminates L1 false sharing across 12 threads.
// 3. Fast line scanning and backward latency parsing.
// 4. Hardware CRC32 + lock-free interning + parallel CSR + select_nth_unstable.
#![allow(unsafe_op_in_unsafe_fn)]
#![allow(dead_code)]
#![allow(static_mut_refs)]

use std::arch::x86_64::*;
use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Barrier;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn write(fd: i32, buf: *const u8, count: usize) -> isize;
    fn memchr(s: *const u8, c: i32, n: usize) -> *const u8;
}

#[repr(C, packed)]
#[derive(Copy, Clone)]
struct Rec {
    lat: u32,
    eid: u16,
}

#[repr(C, align(64))]
#[derive(Copy, Clone)]
struct ThreadState {
    head_blk: [i32; 144],
    cur_blk: [i32; 144],
    cur_fill: [u16; 144],
}

const BLK_SZ: usize = 256;
const NUM_BLKS: usize = 20480;

static mut HR_KEY: [u64; 256] = [0; 256];
static mut EP_KEY: [u64; 8192] = [0; 8192];
static mut G_ERR: [u32; 144 * 2048] = [0; 144 * 2048];
static N_HR: AtomicU32 = AtomicU32::new(0);
static N_EP: AtomicU32 = AtomicU32::new(0);
static N_BLKS: AtomicU32 = AtomicU32::new(0);

static mut HR_PTR: [usize; 144] = [0; 144];
static mut EP_PTR: [usize; 2048] = [0; 2048];
static mut HR_ID: [u8; 256] = [0; 256];
static mut EP_LEN: [u8; 2048] = [0; 2048];
static mut ORD_H: [u8; 144] = [0; 144];
static mut EP_ID: [u16; 8192] = [0; 8192];
static mut ORD_E: [u16; 2048] = [0; 2048];
static mut BLK_FILL: [u16; NUM_BLKS] = [0; NUM_BLKS];
static mut BLK_NEXT: [i32; NUM_BLKS] = [0; NUM_BLKS];
static mut POOL: [Rec; NUM_BLKS * BLK_SZ] = [Rec { lat: 0, eid: 0 }; NUM_BLKS * BLK_SZ];
static mut TS: [ThreadState; 12] = [ThreadState {
    head_blk: [0; 144],
    cur_blk: [0; 144],
    cur_fill: [0; 144],
}; 12];
static mut T_OUT_LEN: [usize; 12] = [0; 12];
static mut T_OUT: [[u8; 360000]; 12] = [[0; 360000]; 12];

#[inline(always)]
unsafe fn atomic_load_u64(p: *const u64, ord: Ordering) -> u64 {
    (*(p as *const AtomicU64)).load(ord)
}

#[inline(always)]
unsafe fn atomic_store_u64(p: *const u64, val: u64, ord: Ordering) {
    (*(p as *const AtomicU64)).store(val, ord)
}

#[inline(always)]
unsafe fn atomic_cas_u64(p: *const u64, exp: u64, val: u64) -> Result<u64, u64> {
    (*(p as *const AtomicU64)).compare_exchange(exp, val, Ordering::AcqRel, Ordering::Acquire)
}

#[inline(always)]
unsafe fn atomic_add_u32(p: *const u32, val: u32, ord: Ordering) -> u32 {
    (*(p as *const AtomicU32)).fetch_add(val, ord)
}

#[inline(always)]
unsafe fn fmt_u32(mut p: *mut u8, mut v: u32) -> *mut u8 {
    let mut buf = [0u8; 12];
    let mut i = 12;
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    let len = 12 - i;
    std::ptr::copy_nonoverlapping(buf.as_ptr().add(i), p, len);
    p.add(len)
}

#[inline(always)]
unsafe fn get_hid(p: *const u8) -> u8 {
    let k1 = (p as *const u64).read_unaligned();
    let k2 = (p.add(5) as *const u64).read_unaligned();
    let mut h = _mm_crc32_u64(k1, k2);
    if h <= 1 {
        h = 2;
    }
    let mut s = (h as usize) & 255;
    loop {
        let p_key = HR_KEY.as_ptr().add(s);
        let k = atomic_load_u64(p_key, Ordering::Acquire);
        if k == h {
            let hid = HR_ID[s];
            let hp = HR_PTR[hid as usize] as *const u8;
            if std::slice::from_raw_parts(hp, 13) == std::slice::from_raw_parts(p, 13) {
                return hid;
            }
        }
        if k == 0 && atomic_cas_u64(p_key, 0, 1).is_ok() {
            let id = N_HR.fetch_add(1, Ordering::Relaxed) as u8;
            HR_PTR[id as usize] = p as usize;
            HR_ID[s] = id;
            atomic_store_u64(p_key, h, Ordering::Release);
            return id;
        }
        while atomic_load_u64(p_key, Ordering::Acquire) == 1 {
            _mm_pause();
        }
        if atomic_load_u64(p_key, Ordering::Acquire) == h {
            let hid = HR_ID[s];
            let hp = HR_PTR[hid as usize] as *const u8;
            if std::slice::from_raw_parts(hp, 13) == std::slice::from_raw_parts(p, 13) {
                return hid;
            }
        }
        s = (s + 1) & 255;
    }
}

#[inline(always)]
unsafe fn get_eid(ep: *const u8, len: u8) -> u16 {
    let mut h = (len as u64) + 1;
    let mut k = 0usize;
    while k + 8 <= len as usize {
        let w = (ep.add(k) as *const u64).read_unaligned();
        h = _mm_crc32_u64(h, w);
        k += 8;
    }
    while k < len as usize {
        h = _mm_crc32_u8(h as u32, *ep.add(k)) as u64;
        k += 1;
    }
    h = (h << 8) | (len as u64) | (1u64 << 62);
    let mut s = ((h >> 8) as usize) & 8191;
    loop {
        let p_key = EP_KEY.as_ptr().add(s);
        let key = atomic_load_u64(p_key, Ordering::Acquire);
        if key == h {
            let eid = EP_ID[s];
            let ep_ptr = EP_PTR[eid as usize] as *const u8;
            if EP_LEN[eid as usize] == len
                && std::slice::from_raw_parts(ep_ptr, len as usize)
                    == std::slice::from_raw_parts(ep, len as usize)
            {
                return eid;
            }
        }
        if key == 0 && atomic_cas_u64(p_key, 0, 1).is_ok() {
            let id = N_EP.fetch_add(1, Ordering::Relaxed) as u16;
            EP_PTR[id as usize] = ep as usize;
            EP_LEN[id as usize] = len;
            EP_ID[s] = id;
            atomic_store_u64(p_key, h, Ordering::Release);
            return id;
        }
        while atomic_load_u64(p_key, Ordering::Acquire) == 1 {
            _mm_pause();
        }
        if atomic_load_u64(p_key, Ordering::Acquire) == h {
            let eid = EP_ID[s];
            let ep_ptr = EP_PTR[eid as usize] as *const u8;
            if EP_LEN[eid as usize] == len
                && std::slice::from_raw_parts(ep_ptr, len as usize)
                    == std::slice::from_raw_parts(ep, len as usize)
            {
                return eid;
            }
        }
        s = (s + 1) & 8191;
    }
}

unsafe fn write_all(fd: i32, mut buf: *const u8, mut count: usize) {
    while count > 0 {
        let n = write(fd, buf, count);
        if n <= 0 {
            break;
        }
        buf = buf.add(n as usize);
        count -= n as usize;
    }
}

fn main() {
    unsafe {
        let mut args = std::env::args();
        let _ = args.next();
        let in_path = args.next().unwrap_or_else(|| "/data/requests.log".into());
        let out_path = args.next().unwrap_or_else(|| "/out/summary.txt".into());

        let in_file = match File::open(&in_path) {
            Ok(f) => f,
            Err(_) => return,
        };
        let out_file = match File::create(&out_path) {
            Ok(f) => f,
            Err(_) => return,
        };
        let sz = in_file.metadata().map(|m| m.len() as usize).unwrap_or(0);
        if sz == 0 {
            return;
        }
        let fd = in_file.as_raw_fd();
        let out_fd = out_file.as_raw_fd();
        let mut base = mmap(std::ptr::null_mut(), sz, 1, 1, fd, 0);
        if base as isize == -1 {
            base = mmap(std::ptr::null_mut(), sz, 1, 2, fd, 0);
        }
        if base as isize == -1 {
            return;
        }

        let bar = Barrier::new(12);
        let base_addr = base as usize;

        std::thread::scope(|scope| {
            for tid in 0..12 {
                let bar_ref = &bar;
                scope.spawn(move || {
                    let b_ptr = base_addr as *const u8;
                    let mut s = if tid > 0 { (tid * sz) / 12 } else { 0 };
                    let mut e = if tid < 11 { ((tid + 1) * sz) / 12 } else { sz };
                    while s > 0 && *b_ptr.add(s - 1) != b'\n' {
                        s += 1;
                    }
                    while e < sz && *b_ptr.add(e - 1) != b'\n' {
                        e += 1;
                    }

                    let my = &mut TS[tid];
                    my.head_blk.fill(-1);
                    my.cur_blk.fill(-1);
                    my.cur_fill.fill(0);
                    let mut last_hr = std::ptr::null::<u8>();
                    let mut hid = 0u8;

                    let mut p = b_ptr.add(s);
                    let end_c = b_ptr.add(e);
                    while p < end_c {
                        let nl = memchr(p, b'\n' as i32, (end_c as usize) - (p as usize));
                        if nl.is_null() {
                            break;
                        }
                        let mut ep = p.add(31);
                        while *ep != b' ' {
                            ep = ep.add(1);
                        }
                        ep = ep.add(1);

                        let mut cur = nl.sub(1);
                        if *cur == b'\r' {
                            cur = cur.sub(1);
                        }
                        let mut lat = 0u32;
                        let mut mult = 1u32;
                        while *cur != b' ' {
                            lat += (*cur - b'0') as u32 * mult;
                            mult *= 10;
                            cur = cur.sub(1);
                        }
                        let elen = (cur as usize - 4) - (ep as usize);
                        if last_hr.is_null()
                            || std::slice::from_raw_parts(p, 13)
                                != std::slice::from_raw_parts(last_hr, 13)
                        {
                            last_hr = p;
                            hid = get_hid(p);
                        }
                        let eid = get_eid(ep, elen as u8);
                        if *cur.sub(3) == b'5' {
                            let p_err = G_ERR.as_ptr().add((hid as usize) * 2048 + (eid as usize));
                            atomic_add_u32(p_err, 1, Ordering::Relaxed);
                        }

                        let mut b = my.cur_blk[hid as usize];
                        let mut f = my.cur_fill[hid as usize] as usize;
                        if b < 0 || f == BLK_SZ {
                            if b >= 0 {
                                BLK_FILL[b as usize] = BLK_SZ as u16;
                            }
                            let nb = N_BLKS.fetch_add(1, Ordering::Relaxed) as i32;
                            BLK_NEXT[nb as usize] = -1;
                            if b < 0 {
                                my.head_blk[hid as usize] = nb;
                            } else {
                                BLK_NEXT[b as usize] = nb;
                            }
                            my.cur_blk[hid as usize] = nb;
                            b = nb;
                            f = 0;
                        }
                        POOL[(b as usize) * BLK_SZ + f] = Rec { lat, eid };
                        my.cur_fill[hid as usize] = (f + 1) as u16;

                        p = nl.add(1);
                    }

                    for h in 0..144 {
                        if my.cur_blk[h] >= 0 {
                            BLK_FILL[my.cur_blk[h] as usize] = my.cur_fill[h];
                        }
                    }

                    bar_ref.wait();

                    if tid == 0 {
                        let nh = N_HR.load(Ordering::Relaxed) as usize;
                        let ne = N_EP.load(Ordering::Relaxed) as usize;
                        for i in 0..nh {
                            ORD_H[i] = i as u8;
                        }
                        for i in 0..ne {
                            ORD_E[i] = i as u16;
                        }
                        ORD_H[..nh].sort_unstable_by(|&a, &b| {
                            let sa = std::slice::from_raw_parts(HR_PTR[a as usize] as *const u8, 13);
                            let sb = std::slice::from_raw_parts(HR_PTR[b as usize] as *const u8, 13);
                            sa.cmp(sb)
                        });
                        ORD_E[..ne].sort_unstable_by(|&a, &b| {
                            let sa = std::slice::from_raw_parts(
                                EP_PTR[a as usize] as *const u8,
                                EP_LEN[a as usize] as usize,
                            );
                            let sb = std::slice::from_raw_parts(
                                EP_PTR[b as usize] as *const u8,
                                EP_LEN[b as usize] as usize,
                            );
                            sa.cmp(sb)
                        });
                    }

                    bar_ref.wait();

                    let nh = N_HR.load(Ordering::Relaxed) as usize;
                    let ne = N_EP.load(Ordering::Relaxed) as usize;
                    let mut cnt = [0u32; 2048];
                    let mut off = [0u32; 2048];
                    let mut lats = Vec::<u32>::with_capacity(65536);

                    let mut base_hi = 0;
                    while base_hi < nh {
                        let hi = base_hi + tid;
                        let mut out_len = 0usize;
                        if hi < nh {
                            let h_id = ORD_H[hi] as usize;
                            cnt[..ne].fill(0);
                            let mut tot = 0u32;
                            for t in 0..12 {
                                let mut b = TS[t].head_blk[h_id];
                                while b >= 0 {
                                    let bf = BLK_FILL[b as usize] as usize;
                                    let base_idx = (b as usize) * BLK_SZ;
                                    for i in 0..bf {
                                        let eid = POOL[base_idx + i].eid as usize;
                                        cnt[eid] += 1;
                                    }
                                    b = BLK_NEXT[b as usize];
                                }
                            }
                            for i in 0..ne {
                                off[i] = tot;
                                tot += cnt[i];
                                cnt[i] = 0;
                            }
                            lats.resize(tot as usize, 0);
                            for t in 0..12 {
                                let mut b = TS[t].head_blk[h_id];
                                while b >= 0 {
                                    let bf = BLK_FILL[b as usize] as usize;
                                    let base_idx = (b as usize) * BLK_SZ;
                                    for i in 0..bf {
                                        let r = POOL[base_idx + i];
                                        let eid = r.eid as usize;
                                        lats[(off[eid] + cnt[eid]) as usize] = r.lat;
                                        cnt[eid] += 1;
                                    }
                                    b = BLK_NEXT[b as usize];
                                }
                            }

                            let out_base = T_OUT[tid].as_mut_ptr();
                            let mut ptr = out_base;
                            for ei in 0..ne {
                                let eid = ORD_E[ei] as usize;
                                let n = cnt[eid] as usize;
                                if n == 0 {
                                    continue;
                                }
                                let s_off = off[eid] as usize;
                                let l_slice = &mut lats[s_off..s_off + n];
                                let i50 = (50 * n + 99) / 100 - 1;
                                let i95 = (95 * n + 99) / 100 - 1;
                                let i99 = (99 * n + 99) / 100 - 1;
                                if n <= 32 {
                                    l_slice.sort_unstable();
                                } else {
                                    l_slice.select_nth_unstable(i50);
                                    if i95 > i50 {
                                        l_slice[i50 + 1..].select_nth_unstable(i95 - i50 - 1);
                                    }
                                    if i99 > i95 {
                                        l_slice[i95 + 1..].select_nth_unstable(i99 - i95 - 1);
                                    }
                                }
                                let err = G_ERR[h_id * 2048 + eid];
                                let ppm = ((err as u64) * 1_000_000 / (n as u64)) as u32;

                                let hp = HR_PTR[h_id] as *const u8;
                                std::ptr::copy_nonoverlapping(hp, ptr, 13);
                                ptr = ptr.add(13);
                                std::ptr::copy_nonoverlapping(b":00:00Z ".as_ptr(), ptr, 8);
                                ptr = ptr.add(8);
                                let el = EP_LEN[eid] as usize;
                                let ep = EP_PTR[eid] as *const u8;
                                std::ptr::copy_nonoverlapping(ep, ptr, el);
                                ptr = ptr.add(el);
                                *ptr = b' ';
                                ptr = ptr.add(1);

                                for v in [
                                    n as u32,
                                    l_slice[i50],
                                    l_slice[i95],
                                    l_slice[i99],
                                    err,
                                    ppm,
                                ] {
                                    ptr = fmt_u32(ptr, v);
                                    *ptr = b' ';
                                    ptr = ptr.add(1);
                                }
                                *ptr.sub(1) = b'\n';
                            }
                            out_len = ptr as usize - out_base as usize;
                        }
                        T_OUT_LEN[tid] = out_len;
                        bar_ref.wait();
                        if tid == 0 {
                            for k in 0..12 {
                                if base_hi + k < nh && T_OUT_LEN[k] > 0 {
                                    write_all(out_fd, T_OUT[k].as_ptr(), T_OUT_LEN[k]);
                                }
                            }
                        }
                        bar_ref.wait();
                        base_hi += 12;
                    }
                });
            }
        });
    }
}
