// Speed Demon 2026 — Track A (Ingest) Championship Solution
#![allow(unsafe_op_in_unsafe_fn, dead_code, unused_unsafe, dangerous_implicit_autorefs, suspicious_runtime_symbol_definitions)]
use std::{arch::x86_64::*, cell::UnsafeCell, fs::File, os::unix::io::AsRawFd, ptr::copy_nonoverlapping as copy, slice::{from_raw_parts as slc, from_raw_parts_mut as slc_mut}, sync::{atomic::{AtomicU32, AtomicU64, Ordering::*}, Barrier}};

extern "C" {
    fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
    fn write(fd: i32, b: *const std::ffi::c_void, c: usize) -> isize;
}

#[repr(transparent)]
struct Global<T>(UnsafeCell<T>);
unsafe impl<T> Sync for Global<T> {}

#[repr(C, align(64))]
#[derive(Copy, Clone)]
struct TState { head: [i32; 80], cur: [i32; 80], fill: [u16; 80] }

const BLK_SZ: usize = 256;
const N_BLK: usize = 14336;

struct SharedData {
    hr_key: [u64; 256], ep_key: [u64; 8192], g_err: [u32; 80 * 2048], n_cnt: [AtomicU32; 4],
    hr_ptr: [usize; 80], ep_ptr: [usize; 2048], ep_len: [u8; 2048], ord_h: [u8; 80], ord_e: [u16; 2048],
    blk_fill: [u16; N_BLK], blk_next: [i32; N_BLK], pool: [u32; N_BLK * BLK_SZ], big_lat: [u32; 262144],
    ts: [TState; 12], t_out_len: [usize; 12], t_out: [[u8; 163840]; 12],
}

static S: Global<SharedData> = Global(UnsafeCell::new(SharedData {
    hr_key: [0; 256], ep_key: [0; 8192], g_err: [0; 80 * 2048],
    n_cnt: [AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)],
    hr_ptr: [0; 80], ep_ptr: [0; 2048], ep_len: [0; 2048], ord_h: [0; 80], ord_e: [0; 2048],
    blk_fill: [0; N_BLK], blk_next: [0; N_BLK], pool: [0; N_BLK * BLK_SZ], big_lat: [0; 262144],
    ts: [TState { head: [0; 80], cur: [0; 80], fill: [0; 80] }; 12], t_out_len: [0; 12], t_out: [[0; 163840]; 12],
}));

#[inline(always)]
unsafe fn fmt_u32_sp(p: *mut u8, mut v: u32) -> *mut u8 {
    let (mut b, mut i) = ([b' '; 12], 11);
    loop { i -= 1; b[i] = b'0' + (v % 10) as u8; v /= 10; if v == 0 { break; } }
    copy(b.as_ptr().add(i), p, 12 - i);
    p.add(12 - i)
}

#[inline(always)]
unsafe fn write_all(fd: i32, mut b: *const u8, mut n: usize) {
    while n > 0 {
        let r = write(fd, b as *const std::ffi::c_void, n);
        if r <= 0 { break; }
        b = b.add(r as usize);
        n -= r as usize;
    }
}

#[cold]
unsafe fn intern(sd: *mut SharedData, is_ep: bool, p: *const u8, len: u8, h: u64) -> u16 {
    let (tab, mask, ci) = if is_ep { ((*sd).ep_key.as_ptr(), 8191, 1) } else { ((*sd).hr_key.as_ptr(), 255, 0) };
    let mut s = ((h >> 16) as usize) & mask;
    loop {
        let cell = &*(tab.add(s) as *const AtomicU64);
        let kv = cell.load(Acquire);
        if (kv ^ h) < 65536 && kv != 0 {
            let id = (kv & 0xFFFF) as u16;
            if is_ep { if (*sd).ep_len[id as usize] == len && slc((*sd).ep_ptr[id as usize] as *const u8, len as usize) == slc(p, len as usize) { return id; } }
            else if slc((*sd).hr_ptr[id as usize] as *const u8, 13) == slc(p, 13) { return id; }
        }
        if kv == 0 && cell.compare_exchange(0, 1, AcqRel, Acquire).is_ok() {
            let id = (*sd).n_cnt[ci].fetch_add(1, Relaxed) as usize;
            if is_ep { (*sd).ep_ptr[id] = p as usize; (*sd).ep_len[id] = len; } else { (*sd).hr_ptr[id] = p as usize; }
            cell.store(h | (id as u64), Release);
            return id as u16;
        }
        while cell.load(Acquire) == 1 { _mm_pause(); }
        let kv2 = cell.load(Acquire);
        if (kv2 ^ h) < 65536 && kv2 != 0 {
            let id = (kv2 & 0xFFFF) as u16;
            if is_ep { if (*sd).ep_len[id as usize] == len && slc((*sd).ep_ptr[id as usize] as *const u8, len as usize) == slc(p, len as usize) { return id; } }
            else if slc((*sd).hr_ptr[id as usize] as *const u8, 13) == slc(p, 13) { return id; }
        }
        s = (s + 1) & mask;
    }
}

fn main() {
    unsafe {
        let mut args = std::env::args().skip(1);
        let in_p = args.next().unwrap_or_else(|| "/data/requests.log".into());
        let out_p = args.next().unwrap_or_else(|| "/out/summary.txt".into());
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
                let bar_ref = &bar;
                sc.spawn(move || {
                    let sd = S.0.get();
                    let b_ptr = base_addr as *const u8;
                    let (mut s, mut e) = (if tid > 0 { (tid * sz) / 12 } else { 0 }, if tid < 11 { ((tid + 1) * sz) / 12 } else { sz });
                    if s > 0 { s += slc(b_ptr.add(s - 1), sz - s + 1).iter().position(|&c| c == b'\n').unwrap(); }
                    if e < sz { e += slc(b_ptr.add(e - 1), sz - e + 1).iter().position(|&c| c == b'\n').unwrap(); }
                    let my = &mut (*sd).ts[tid];
                    *my = TState { head: [-1; 80], cur: [-1; 80], fill: [0; 80] };
                    let (mut last_hw, mut hid, mut tl_ep) = (0u64, 0usize, [0u64; 512]);
                    let (mut p, end_c, sp_v) = (b_ptr.add(s), b_ptr.add(e), _mm256_set1_epi8(b' ' as i8));

                    while p < end_c {
                        let hw = (p.add(5) as *const u64).read_unaligned();
                        if hw != last_hw {
                            last_hw = hw;
                            let hh = (((_mm_crc32_u64(0, (p as *const u64).read_unaligned()) << 32) ^ hw.wrapping_mul(0x9e3779b97f4a7c15)) & !0xFFFF) | (1 << 63);
                            hid = intern(sd, false, p, 13, hh) as usize;
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
                            let id = intern(sd, true, ep, elen as u8, h);
                            tl_ep[c_idx] = h | (id as u64);
                            id
                        };
                        let st_p = ep.add(elen + 1);
                        if *st_p == b'5' {
                            let err_cell = (*sd).g_err.as_ptr().add(hid * 2048 + (eid as usize)) as *const AtomicU32;
                            (*err_cell).fetch_add(1, Relaxed);
                        }
                        let (mut c, mut lat) = (st_p.add(5), (*st_p.add(4) - b'0') as u32);
                        while *c >= b'0' { (lat, c) = (lat * 10 + (*c - b'0') as u32, c.add(1)); }
                        p = c.add(if *c == b'\r' { 2 } else { 1 });

                        let (mut b, mut f) = (my.cur[hid], my.fill[hid] as usize);
                        if b < 0 || f == BLK_SZ {
                            if b >= 0 { (*sd).blk_fill[b as usize] = BLK_SZ as u16; }
                            let nb = (*sd).n_cnt[2].fetch_add(1, Relaxed) as i32;
                            (*sd).blk_next[nb as usize] = -1;
                            if b < 0 { my.head[hid] = nb; } else { (*sd).blk_next[b as usize] = nb; }
                            (my.cur[hid], b, f) = (nb, nb, 0);
                        }
                        let pk = if lat < (1 << 20) { (lat << 11) | (eid as u32) } else {
                            let bi = (*sd).n_cnt[3].fetch_add(1, Relaxed);
                            (*sd).big_lat[bi as usize] = lat;
                            0x8000_0000 | (bi << 11) | (eid as u32)
                        };
                        ((*sd).pool[(b as usize) * BLK_SZ + f], my.fill[hid]) = (pk, (f + 1) as u16);
                    }
                    for h in 0..80 { if my.cur[h] >= 0 { (*sd).blk_fill[my.cur[h] as usize] = my.fill[h]; } }
                    bar_ref.wait();

                    if tid == 0 {
                        let (nh, ne) = ((*sd).n_cnt[0].load(Relaxed) as usize, (*sd).n_cnt[1].load(Relaxed) as usize);
                        for i in 0..nh { (*sd).ord_h[i] = i as u8; }
                        for i in 0..ne { (*sd).ord_e[i] = i as u16; }
                        slc_mut((*sd).ord_h.as_mut_ptr(), nh).sort_unstable_by(|&x, &y| {
                            slc((*sd).hr_ptr[x as usize] as *const u8, 13).cmp(slc((*sd).hr_ptr[y as usize] as *const u8, 13))
                        });
                        slc_mut((*sd).ord_e.as_mut_ptr(), ne).sort_unstable_by(|&x, &y| {
                            slc((*sd).ep_ptr[x as usize] as *const u8, (*sd).ep_len[x as usize] as usize)
                                .cmp(slc((*sd).ep_ptr[y as usize] as *const u8, (*sd).ep_len[y as usize] as usize))
                        });
                    }
                    bar_ref.wait();

                    let (nh, ne) = ((*sd).n_cnt[0].load(Relaxed) as usize, (*sd).n_cnt[1].load(Relaxed) as usize);
                    let (mut cnt, mut off, mut lats, mut base_hi) = ([0u32; 2048], [0u32; 2048], Vec::<u32>::new(), 0);
                    while base_hi < nh {
                        let (hi, mut out_len) = (base_hi + tid, 0usize);
                        if hi < nh {
                            let (h_id, mut tot) = ((*sd).ord_h[hi] as usize, 0u32);
                            cnt[..ne].fill(0);
                            for t in 0..12 {
                                let mut b = (*sd).ts[t].head[h_id];
                                while b >= 0 {
                                    let (bf, base_idx) = ((*sd).blk_fill[b as usize] as usize, (b as usize) * BLK_SZ);
                                    for i in 0..bf { cnt[((*sd).pool[base_idx + i] & 2047) as usize] += 1; }
                                    b = (*sd).blk_next[b as usize];
                                }
                            }
                            for i in 0..ne { (off[i], tot, cnt[i]) = (tot, tot + cnt[i], 0); }
                            lats.resize(tot as usize, 0);
                            for t in 0..12 {
                                let mut b = (*sd).ts[t].head[h_id];
                                while b >= 0 {
                                    let (bf, base_idx) = ((*sd).blk_fill[b as usize] as usize, (b as usize) * BLK_SZ);
                                    for i in 0..bf {
                                        let v = (*sd).pool[base_idx + i];
                                        let (eid, lat) = ((v & 2047) as usize, if (v as i32) >= 0 { v >> 11 } else { (*sd).big_lat[((v >> 11) & 0xF_FFFF) as usize] });
                                        lats[(off[eid] + cnt[eid]) as usize] = lat;
                                        cnt[eid] += 1;
                                    }
                                    b = (*sd).blk_next[b as usize];
                                }
                            }
                            let out_b = (*sd).t_out[tid].as_mut_ptr();
                            let mut ptr = out_b;
                            for ei in 0..ne {
                                let (eid, n) = ((*sd).ord_e[ei] as usize, cnt[(*sd).ord_e[ei] as usize] as usize);
                                if n == 0 { continue; }
                                let l = &mut lats[off[eid] as usize..off[eid] as usize + n];
                                let (i50, i95, i99) = ((50 * n + 99) / 100 - 1, (95 * n + 99) / 100 - 1, (99 * n + 99) / 100 - 1);
                                if n <= 32 { l.sort_unstable(); } else {
                                    l.select_nth_unstable(i50);
                                    if i95 > i50 { l[i50 + 1..].select_nth_unstable(i95 - i50 - 1); }
                                    if i99 > i95 { l[i95 + 1..].select_nth_unstable(i99 - i95 - 1); }
                                }
                                let (err, el) = ((*sd).g_err[h_id * 2048 + eid], (*sd).ep_len[eid] as usize);
                                let ppm = ((err as u64) * 1_000_000 / (n as u64)) as u32;
                                copy((*sd).hr_ptr[h_id] as *const u8, ptr, 13);
                                copy(b":00:00Z ".as_ptr(), ptr.add(13), 8);
                                copy((*sd).ep_ptr[eid] as *const u8, ptr.add(21), el);
                                *ptr.add(21 + el) = b' ';
                                ptr = ptr.add(22 + el);
                                for v in [n as u32, l[i50], l[i95], l[i99], err, ppm] { ptr = fmt_u32_sp(ptr, v); }
                                *ptr.sub(1) = b'\n';
                            }
                            out_len = ptr as usize - out_b as usize;
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
