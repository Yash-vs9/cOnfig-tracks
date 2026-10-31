// Speed Demon 2026 — Track A (Ingest) Championship Rust Solution
// Zero external dependencies (D = 1), 100% .bss zero-initialized static arena, offline build (--network=none).
// 1. Zero-copy mmap(MAP_SHARED) over pre-warmed /data/requests.log (0 B cgroup charge).
// 2. Cache-line aligned ThreadState (#[repr(C, align(64))]) eliminates L1 false sharing across 12 threads.
// 3. Fast line scanning and backward latency parsing.
// 4. Hardware CRC32 + lock-free interning + parallel CSR + select_nth_unstable.
#![allow(unsafe_op_in_unsafe_fn)]
#![allow(dead_code)]

use std::arch::x86_64::*;
use std::cell::UnsafeCell;
use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Barrier;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn write(fd: i32, buf: *const u8, count: usize) -> isize;
    fn memchr(s: *const u8, c: i32, n: usize) -> *const u8;
}

#[repr(transparent)]
struct Global<T>(UnsafeCell<T>);
unsafe impl<T> Sync for Global<T> {}

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

const ZERO_U64: AtomicU64 = AtomicU64::new(0);
const ZERO_U32: AtomicU32 = AtomicU32::new(0);

static HR_KEY: [AtomicU64; 256] = [ZERO_U64; 256];
static EP_KEY: [AtomicU64; 8192] = [ZERO_U64; 8192];
static G_ERR: [AtomicU32; 144 * 2048] = [ZERO_U32; 144 * 2048];
static N_HR: AtomicU32 = AtomicU32::new(0);
static N_EP: AtomicU32 = AtomicU32::new(0);
static N_BLKS: AtomicU32 = AtomicU32::new(0);

struct SharedData {
    hr_ptr: [*const u8; 144],
    ep_ptr: [*const u8; 2048],
    hr_id: [u8; 256],
    ep_len: [u8; 2048],
    ord_h: [u8; 144],
    ep_id: [u16; 8192],
    ord_e: [u16; 2048],
    blk_fill: [u16; NUM_BLKS],
    blk_next: [i32; NUM_BLKS],
    pool: [Rec; NUM_BLKS * BLK_SZ],
    ts: [ThreadState; 12],
    t_out_len: [usize; 12],
    t_out: [[u8; 360000]; 12],
}

static S: Global<SharedData> = Global(UnsafeCell::new(SharedData {
    hr_ptr: [std::ptr::null(); 144],
    ep_ptr: [std::ptr::null(); 2048],
    hr_id: [0; 256],
    ep_len: [0; 2048],
    ord_h: [0; 144],
    ep_id: [0; 8192],
    ord_e: [0; 2048],
    blk_fill: [0; NUM_BLKS],
    blk_next: [0; NUM_BLKS],
    pool: [Rec { lat: 0, eid: 0 }; NUM_BLKS * BLK_SZ],
    ts: [ThreadState {
        head_blk: [0; 144],
        cur_blk: [0; 144],
        cur_fill: [0; 144],
    }; 12],
    t_out_len: [0; 12],
    t_out: [[0; 360000]; 12],
}));

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
unsafe fn get_hid(p: *const u8, sd: *mut SharedData) -> u8 {
    let k1 = (p as *const u64).read_unaligned();
    let k2 = (p.add(5) as *const u64).read_unaligned();
    let mut h = _mm_crc32_u64(k1, k2);
    if h <= 1 {
        h = 2;
    }
    let mut s = (h as usize) & 255;
    loop {
        let k = HR_KEY[s].load(Ordering::Acquire);
        if k == h {
            let hid = (*sd).hr_id[s];
            if std::slice::from_raw_parts((*sd).hr_ptr[hid as usize], 13)
                == std::slice::from_raw_parts(p, 13)
            {
                return hid;
            }
        }
        if k == 0
            && HR_KEY[s]
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            let id = N_HR.fetch_add(1, Ordering::Relaxed) as u8;
            (*sd).hr_ptr[id as usize] = p;
            (*sd).hr_id[s] = id;
            HR_KEY[s].store(h, Ordering::Release);
            return id;
        }
        while HR_KEY[s].load(Ordering::Acquire) == 1 {
            std::hint::spin_loop();
        }
        if HR_KEY[s].load(Ordering::Acquire) == h {
            let hid = (*sd).hr_id[s];
            if std::slice::from_raw_parts((*sd).hr_ptr[hid as usize], 13)
                == std::slice::from_raw_parts(p, 13)
            {
                return hid;
            }
        }
        s = (s + 1) & 255;
    }
}

#[inline(always)]
unsafe fn get_eid(ep: *const u8, len: u8, sd: *mut SharedData) -> u16 {
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
        let key = EP_KEY[s].load(Ordering::Acquire);
        if key == h {
            let eid = (*sd).ep_id[s];
            if (*sd).ep_len[eid as usize] == len
                && std::slice::from_raw_parts((*sd).ep_ptr[eid as usize], len as usize)
                    == std::slice::from_raw_parts(ep, len as usize)
            {
                return eid;
            }
        }
        if key == 0
            && EP_KEY[s]
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            let id = N_EP.fetch_add(1, Ordering::Relaxed) as u16;
            (*sd).ep_ptr[id as usize] = ep;
            (*sd).ep_len[id as usize] = len;
            (*sd).ep_id[s] = id;
            EP_KEY[s].store(h, Ordering::Release);
            return id;
        }
        while EP_KEY[s].load(Ordering::Acquire) == 1 {
            std::hint::spin_loop();
        }
        if EP_KEY[s].load(Ordering::Acquire) == h {
            let eid = (*sd).ep_id[s];
            if (*sd).ep_len[eid as usize] == len
                && std::slice::from_raw_parts((*sd).ep_ptr[eid as usize], len as usize)
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
                    let sd = S.0.get();
                    let b_ptr = base_addr as *const u8;
                    let mut s = if tid > 0 { (tid * sz) / 12 } else { 0 };
                    let mut e = if tid < 11 { ((tid + 1) * sz) / 12 } else { sz };
                    while s > 0 && *b_ptr.add(s - 1) != b'\n' {
                        s += 1;
                    }
                    while e < sz && *b_ptr.add(e - 1) != b'\n' {
                        e += 1;
                    }

                    let my = &mut (*sd).ts[tid];
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
                            hid = get_hid(p, sd);
                        }
                        let eid = get_eid(ep, elen as u8, sd);
                        if *cur.sub(3) == b'5' {
                            G_ERR[(hid as usize) * 2048 + (eid as usize)]
                                .fetch_add(1, Ordering::Relaxed);
                        }

                        let mut b = my.cur_blk[hid as usize];
                        let mut f = my.cur_fill[hid as usize] as usize;
                        if b < 0 || f == BLK_SZ {
                            if b >= 0 {
                                (*sd).blk_fill[b as usize] = BLK_SZ as u16;
                            }
                            let nb = N_BLKS.fetch_add(1, Ordering::Relaxed) as i32;
                            (*sd).blk_next[nb as usize] = -1;
                            if b < 0 {
                                my.head_blk[hid as usize] = nb;
                            } else {
                                (*sd).blk_next[b as usize] = nb;
                            }
                            my.cur_blk[hid as usize] = nb;
                            b = nb;
                            f = 0;
                        }
                        (*sd).pool[(b as usize) * BLK_SZ + f] = Rec { lat, eid };
                        my.cur_fill[hid as usize] = (f + 1) as u16;

                        p = nl.add(1);
                    }

                    for h in 0..144 {
                        if my.cur_blk[h] >= 0 {
                            (*sd).blk_fill[my.cur_blk[h] as usize] = my.cur_fill[h];
                        }
                    }

                    bar_ref.wait();

                    if tid == 0 {
                        let nh = N_HR.load(Ordering::Relaxed) as usize;
                        let ne = N_EP.load(Ordering::Relaxed) as usize;
                        for i in 0..nh {
                            (*sd).ord_h[i] = i as u8;
                        }
                        for i in 0..ne {
                            (*sd).ord_e[i] = i as u16;
                        }
                        (*sd).ord_h[..nh].sort_unstable_by(|&a, &b| {
                            let sa = std::slice::from_raw_parts((*sd).hr_ptr[a as usize], 13);
                            let sb = std::slice::from_raw_parts((*sd).hr_ptr[b as usize], 13);
                            sa.cmp(sb)
                        });
                        (*sd).ord_e[..ne].sort_unstable_by(|&a, &b| {
                            let sa = std::slice::from_raw_parts(
                                (*sd).ep_ptr[a as usize],
                                (*sd).ep_len[a as usize] as usize,
                            );
                            let sb = std::slice::from_raw_parts(
                                (*sd).ep_ptr[b as usize],
                                (*sd).ep_len[b as usize] as usize,
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
                            let h_id = (*sd).ord_h[hi] as usize;
                            cnt[..ne].fill(0);
                            let mut tot = 0u32;
                            for t in 0..12 {
                                let mut b = (*sd).ts[t].head_blk[h_id];
                                while b >= 0 {
                                    let bf = (*sd).blk_fill[b as usize] as usize;
                                    let base_idx = (b as usize) * BLK_SZ;
                                    for i in 0..bf {
                                        let eid = (*sd).pool[base_idx + i].eid as usize;
                                        cnt[eid] += 1;
                                    }
                                    b = (*sd).blk_next[b as usize];
                                }
                            }
                            for i in 0..ne {
                                off[i] = tot;
                                tot += cnt[i];
                                cnt[i] = 0;
                            }
                            lats.resize(tot as usize, 0);
                            for t in 0..12 {
                                let mut b = (*sd).ts[t].head_blk[h_id];
                                while b >= 0 {
                                    let bf = (*sd).blk_fill[b as usize] as usize;
                                    let base_idx = (b as usize) * BLK_SZ;
                                    for i in 0..bf {
                                        let r = (*sd).pool[base_idx + i];
                                        let eid = r.eid as usize;
                                        lats[(off[eid] + cnt[eid]) as usize] = r.lat;
                                        cnt[eid] += 1;
                                    }
                                    b = (*sd).blk_next[b as usize];
                                }
                            }

                            let out_base = (*sd).t_out[tid].as_mut_ptr();
                            let mut ptr = out_base;
                            for ei in 0..ne {
                                let eid = (*sd).ord_e[ei] as usize;
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
                                let err = G_ERR[h_id * 2048 + eid].load(Ordering::Relaxed);
                                let ppm = ((err as u64) * 1_000_000 / (n as u64)) as u32;

                                std::ptr::copy_nonoverlapping((*sd).hr_ptr[h_id], ptr, 13);
                                ptr = ptr.add(13);
                                std::ptr::copy_nonoverlapping(b":00:00Z ".as_ptr(), ptr, 8);
                                ptr = ptr.add(8);
                                let el = (*sd).ep_len[eid] as usize;
                                std::ptr::copy_nonoverlapping((*sd).ep_ptr[eid], ptr, el);
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
                        (*sd).t_out_len[tid] = out_len;
                        bar_ref.wait();
                        if tid == 0 {
                            for k in 0..12 {
                                if base_hi + k < nh && (*sd).t_out_len[k] > 0 {
                                    write_all(out_fd, (*sd).t_out[k].as_ptr(), (*sd).t_out_len[k]);
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
