#![allow(unsafe_op_in_unsafe_fn, dead_code, unused_unsafe, dangerous_implicit_autorefs)]
use std::arch::x86_64::{_mm256_loadu_si256 as ld, _mm_crc32_u64 as crc, *};
use std::ptr::copy_nonoverlapping as copy;
use std::slice::{from_raw_parts as slc, from_raw_parts_mut as slc_mut};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::*};
use std::{cell::UnsafeCell, fs::File, os::unix::io::AsRawFd, sync::Barrier, thread::Builder};

extern "C" {
    fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
    fn memchr(s: *const u8, c: i32, n: usize) -> *const u8;
    fn writev(fd: i32, iov: *const [usize; 2], cnt: i32) -> isize;
    fn mallopt(p: i32, v: i32) -> i32;
}

#[repr(transparent)] struct Global<T>(UnsafeCell<T>);
unsafe impl<T> Sync for Global<T> {}

static DIGITS: &[u8; 200] = b"\
0001020304050607080910111213141516171819\
2021222324252627282930313233343536373839\
4041424344454647484950515253545556575859\
6061626364656667686970717273747576777879\
8081828384858687888990919293949596979899";

#[inline(always)]
unsafe fn fmt_u32_sp(p: *mut u8, mut v: u32) -> *mut u8 {
    let (mut b, mut i) = ([b' '; 12], 11);
    while v >= 100 {
        let r = ((v % 100) * 2) as usize;
        (v, i) = (v / 100, i - 2);
        copy(DIGITS.as_ptr().add(r), b.as_mut_ptr().add(i), 2);
    }
    if v < 10 {
        i -= 1;
        b[i] = b'0' + v as u8;
    } else {
        i -= 2;
        copy(DIGITS.as_ptr().add((v * 2) as usize), b.as_mut_ptr().add(i), 2);
    }
    copy(b.as_ptr().add(i), p, 12 - i);
    p.add(12 - i)
}

#[inline(always)]
unsafe fn hour_to_idx(p: *const u8) -> i32 {
    let (m, d, hr) = ((*p.add(1) - b'0') as i32, ((*p.add(3) - b'0') * 10 + (*p.add(4) - b'0')) as i32, ((*p.add(6) - b'0') * 10 + (*p.add(7) - b'0')) as i32);
    (if m == 0 { d - 30 } else { d + 1 }) * 24 + hr - 18
}

const BLK_PER_TH: usize = 256;
const NUM_TH: usize = 12;
const TOTAL_BLKS: usize = NUM_TH * BLK_PER_TH;

#[repr(C, align(64))]
struct SharedData {
    k: [AtomicU64; 32768],
    c: AtomicU32,
    p: [usize; 2048],
    u_len: [u8; 2048],
    w_sort: [u16; 2048],
    q: [u32; TOTAL_BLKS * 256],
    g: [u32; 32768],
    n: [i32; TOTAL_BLKS],
    h: [[[i32; 12]; NUM_TH]; 2],
    w_blk: [[u16; 12]; NUM_TH],
    l: [[u32; 75000]; 4],
    u_out: [[u8; 163840]; 4],
    out_lens: [usize; 4],
    b_start: [usize; 6],
    b_end: [usize; 6],
    hr_pfx: [[u8; 21]; 72],
}
static S: Global<SharedData> = Global(UnsafeCell::new(unsafe { std::mem::zeroed() }));

#[inline]
#[target_feature(enable = "avx2,sse4.2,bmi1,bmi2")]
unsafe fn intern_ep(p: *const u8, len: u8, h: u64) -> u64 {
    let s = S.0.get();
    let mut idx = ((h >> 16) as usize) & 32767;
    loop {
        let slot = &(*s).k[idx];
        let kv = slot.load(Relaxed);
        if (kv ^ h) < 65536 { return kv; }
        if kv == 0 && slot.compare_exchange(0, 1, AcqRel, Acquire).is_ok() {
            let j = (*s).c.fetch_add(1, Relaxed) as usize;
            ((*s).p[j], (*s).u_len[j], (*s).w_sort[j]) = (p as usize, len, j as u16);
            let full_kv = h | (j as u64);
            slot.store(full_kv, Release);
            return full_kv;
        }
        idx = (idx + (kv > 1) as usize) & 32767;
    }
}

#[target_feature(enable = "avx2,sse4.2,bmi1,bmi2")]
unsafe fn work(t: usize, bp: *const u8, sz: usize, out_fd: i32, bar: &Barrier) {
    let (s, lim, sv) = (S.0.get(), bp as usize + sz, _mm256_set1_epi8(b' ' as i8));
    let u8_at = |q: *const u8| (q as *const u64).read_unaligned();
    let find = |q: *const u8, b: u8| memchr(q, b as _, lim - q as usize) as usize - q as usize;
    let sk = |base: usize, len: usize, i: usize| {
        let off = base + i * len / NUM_TH;
        bp.add(if off > 0 && off < sz { off + find(bp.add(off - 1), b'\n') } else { off })
    };
    let sp_m = |q: *const u8| _mm256_movemask_epi8(_mm256_cmpeq_epi8(ld(q as _), sv)) as u32;

    let mut ep_c = [0u64; 4096];

    for batch in 0..6 {
        for h in 0..12 {
            ((*s).h[0][t][h], (*s).h[1][t][h], (*s).w_blk[t][h]) = (-1, -1, 256);
        }
        let (mut cur_b, mut cur_f) = ([-1i32; 12], [256usize; 12]);
        let (mut next_blk, mut next_hi) = (t * BLK_PER_TH, t * 2048);

        let (b_start, b_end) = ((*s).b_start[batch], (*s).b_end[batch]);
        let (mut p, ec) = (sk(b_start, b_end.saturating_sub(b_start), t), sk(b_start, b_end.saturating_sub(b_start), t + 1));
        let (mut lhw, mut lhid, min_h) = (0u64, -1i32, (batch * 12) as i32);

        while p < ec {
            let hw = u8_at(p.add(5));
            let hid = if hw == lhw { lhid } else { lhw = hw; lhid = hour_to_idx(p.add(5)); lhid };
            let loc_h = hid - min_h;
            if loc_h < 0 || loc_h >= 12 {
                let nl = memchr(p, b'\n' as _, lim - p as usize);
                p = if !nl.is_null() { nl.add(1) } else { break; };
                continue;
            }
            let lh = loc_h as usize;

            let x = u8_at(p.add(32)) ^ 0x2F2F_2F2F_2F2F_2F2F;
            let ep = p.add(32 + ((x.wrapping_sub(0x0101_0101_0101_0101) & !x & 0x8080_8080_8080_8080).trailing_zeros() >> 3) as usize);
            let m = sp_m(ep);
            let el = if m != 0 { m.trailing_zeros() as usize } else { find(ep, b' ') };
            let (d, m8) = (el.saturating_sub(8), !0u64 >> ((8 - el.min(8)) * 8));
            let rd = |o: usize| u8_at(ep.add(o)) & m8;
            let (w0, w1, w2, w3) = (rd(0), rd(d >> 1), rd(d.saturating_sub(8)), rd(d));
            let c1 = crc(crc(el as u64, w0), w2);
            let c2 = crc(crc(el as u64 ^ 0x811c9dc5, w1), w3);
            let h = ((c1 << 32) | c2).wrapping_mul(0x9e3779b97f4a7c15);
            let h = ((h ^ (h >> 29)) & !0xFFFF) | (1 << 63);

            let ci = (((h >> 16) as usize) & 2047) * 2;
            let (kv0, kv1) = (ep_c[ci], ep_c[ci + 1]);
            let kv = if (kv0 ^ h) < 65536 {
                kv0
            } else if (kv1 ^ h) < 65536 {
                ep_c[ci + 1] = kv0; ep_c[ci] = kv1; kv1
            } else {
                let nkv = intern_ep(ep, el as u8, h);
                ep_c[ci + 1] = kv0; ep_c[ci] = nkv; nkv
            };
            let eid = (kv & 0xFFFF) as usize;

            let st_p = ep.add(el + 1);
            let is_err = (*st_p == b'5') as u32;
            let lp = st_p.add(4);
            let sub = u8_at(lp).wrapping_sub(0x3030_3030_3030_3030);
            let mask = sub & 0x8080_8080_8080_8080;
            let (c, lat) = if mask != 0 {
                let tz = mask.trailing_zeros();
                let w = (((sub << 8) << (tz ^ 63)).wrapping_mul(2561) >> 8) & 0x00FF_00FF_00FF_00FF;
                let m64 = (w.wrapping_mul(6553601) >> 16) & 0xFFFF_0000_FFFF;
                (lp.add((tz >> 3) as usize), (m64.wrapping_mul(42949672960001) >> 32) as u32)
            } else {
                let (mut c, mut lat) = (lp.add(1), (*lp - b'0') as u32);
                while *c >= b'0' { (lat, c) = ((lat << 3) + (lat << 1) + (*c - b'0') as u32, c.add(1)); }
                (c, lat)
            };
            p = c.add(1);

            let (mut f, mut b) = (cur_f[lh], cur_b[lh]);
            if f == 256 {
                if b >= 0 { (*s).w_blk[t][lh] = 256; }
                let nb = next_blk as i32; next_blk += 1;
                (*s).n[nb as usize] = -1;
                if b < 0 { (*s).h[0][t][lh] = nb; } else { (*s).n[b as usize] = nb; }
                (b, f) = (nb, 0);
                cur_b[lh] = b;
            }
            let mut hb = lat;
            if lat >= 524288 {
                let gi = next_hi; next_hi += 1;
                hb = 0x8_0000 | (gi as u32);
                (*s).g[gi] = lat;
            }
            let word = (hb << 12) | (is_err << 11) | (eid as u32);
            (*s).q[b as usize * 256 + f] = word;
            cur_f[lh] = f + 1;
        }

        for h in 0..12 {
            if cur_b[h] >= 0 {
                (*s).h[1][t][h] = cur_b[h];
                (*s).w_blk[t][h] = cur_f[h] as u16;
            }
        }

        bar.wait();

        let ne = (*s).c.load(Relaxed) as usize;
        if t == 0 {
            let (p_arr, u_len_arr) = ((*s).p.as_ptr(), (*s).u_len.as_ptr());
            let gs = |i: u16| slc(*p_arr.add(i as usize) as *const u8, *u_len_arr.add(i as usize) as _);
            (*s).w_sort[..ne].sort_unstable_by(|&x, &y| gs(x).cmp(gs(y)));
        }

        bar.wait();

        let g_ptr = (*s).g.as_ptr();
        let val = |v: u32| if (v as i32) < 0 { *g_ptr.add(((v >> 12) & 0x7_FFFF) as usize) } else { v >> 12 };

        for round in 0..3 {
            let lh = round * 4 + t;
            if t < 4 {
                let abs_hour = batch * 12 + lh;
                let (mut cnt, mut off, mut err) = ([0u32; 2048], [0u32; 2048], [0u32; 2048]);
                let lp = (*s).l[t].as_mut_ptr();

                for tr in 0..NUM_TH {
                    let mut b = (*s).h[0][tr][lh];
                    while b >= 0 {
                        let bu = b as usize;
                        let end_f = if (*s).n[bu] < 0 { (*s).w_blk[tr][lh] as usize } else { 256 };
                        for &v in &(*s).q[bu * 256..bu * 256 + end_f] {
                            let e = (v & 2047) as usize;
                            cnt[e] += 1;
                            err[e] += (v >> 11) & 1;
                        }
                        b = (*s).n[bu];
                    }
                }

                let mut tot = 0u32;
                for i in 0..ne { (off[i], tot, cnt[i]) = (tot, tot + cnt[i], 0); }

                for tr in 0..NUM_TH {
                    let mut b = (*s).h[0][tr][lh];
                    while b >= 0 {
                        let bu = b as usize;
                        let end_f = if (*s).n[bu] < 0 { (*s).w_blk[tr][lh] as usize } else { 256 };
                        for &v in &(*s).q[bu * 256..bu * 256 + end_f] {
                            let e = (v & 2047) as usize;
                            *lp.add((off[e] + cnt[e]) as usize) = val(v);
                            cnt[e] += 1;
                        }
                        b = (*s).n[bu];
                    }
                }

                let out_b = (*s).u_out[t].as_mut_ptr();
                let mut ptr = out_b;
                let hr_pfx = &(*s).hr_pfx[abs_hour];

                for &eid_u in (*s).w_sort[..ne].iter().filter(|&&e| cnt[e as usize] > 0) {
                    let (eid, n) = (eid_u as usize, cnt[eid_u as usize] as usize);
                    let l = slc_mut(lp.add(off[eid] as usize), n);
                    let pct = |p: usize| (p * n + 99) / 100 - 1;
                    let (i50, i95, i99) = (pct(50), pct(95), pct(99));

                    if n <= 3 {
                        if n == 2 && l[0] > l[1] { l.swap(0, 1); }
                        else if n == 3 {
                            if l[0] > l[1] { l.swap(0, 1); }
                            if l[1] > l[2] { l.swap(1, 2); if l[0] > l[1] { l.swap(0, 1); } }
                        }
                    } else if n <= 48 {
                        for i in 1..n {
                            let (x, mut j) = (l[i], i);
                            while j > 0 && l[j - 1] > x { l[j] = l[j - 1]; j -= 1; }
                            l[j] = x;
                        }
                    } else {
                        l.select_nth_unstable(i50);
                        if i95 > i50 { l[i50 + 1..].select_nth_unstable(i95 - i50 - 1); }
                        if i99 > i95 { l[i95 + 1..].select_nth_unstable(i99 - i95 - 1); }
                    }

                    let (err_c, el) = (err[eid], (*s).u_len[eid] as usize);
                    copy(hr_pfx.as_ptr(), ptr, 21);
                    copy((*s).p[eid] as *const u8, ptr.add(21), el);
                    *ptr.add(21 + el) = b' ';
                    ptr = ptr.add(22 + el);
                    let ppm = if err_c == 0 { 0 } else { ((err_c as u64) * 1_000_000 / (n as u64)) as u32 };
                    for v in [n as u32, l[i50], l[i95], l[i99], err_c, ppm] { ptr = fmt_u32_sp(ptr, v); }
                    *ptr.sub(1) = b'\n';
                }

                (*s).out_lens[t] = ptr as usize - out_b as usize;
            }

            bar.wait();

            if t == 0 {
                let mut iov = [[0usize; 2]; 4];
                for k in 0..4 { iov[k] = [(*s).u_out[k].as_ptr() as usize, (*s).out_lens[k]]; }
                writev(out_fd, iov.as_ptr(), 4);
            }

            bar.wait();
        }
    }
}

unsafe fn compute_bounds(bp: *const u8, sz: usize) {
    let s = S.0.get();
    for (i, pfx) in (*s).hr_pfx.iter_mut().enumerate() {
        let (m, day, hr) = if i < 6 { (10, 30, 18 + i) } else if i < 30 { (10, 31, i - 6) } else if i < 54 { (11, 1, i - 30) } else { (11, 2, i - 54) };
        *pfx = *b"2026-00-00T00:00:00Z ";
        (pfx[5], pfx[6]) = (b'0' + (m / 10) as u8, b'0' + (m % 10) as u8);
        (pfx[8], pfx[9]) = (b'0' + (day / 10) as u8, b'0' + (day % 10) as u8);
        (pfx[11], pfx[12]) = (b'0' + (hr / 10) as u8, b'0' + (hr % 10) as u8);
    }

    let margin = (sz / 100).clamp(16384, 2097152);
    let mut raw = [0; 7]; raw[6] = sz;
    for (i, &target) in [
        u64::from_be_bytes(*b"10-31T06"),
        u64::from_be_bytes(*b"10-31T18"),
        u64::from_be_bytes(*b"11-01T06"),
        u64::from_be_bytes(*b"11-01T18"),
        u64::from_be_bytes(*b"11-02T06"),
    ].iter().enumerate() {
        let (mut lo, mut hi) = (0, sz);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let mut p = bp.add(mid);
            while p < bp.add(sz) && *p != b'\n' { p = p.add(1); }
            if p >= bp.add(sz.saturating_sub(14)) { hi = mid; continue; }
            let line = p.add(1);
            let h = u64::from_be((line.add(5) as *const u64).read_unaligned());
            if h < target { lo = line as usize - bp as usize; } else { hi = mid; }
        }
        let mut p = bp.add(lo.saturating_sub(500));
        while p < bp.add(sz) && *p != b'\n' { p = p.add(1); }
        while p < bp.add(sz.saturating_sub(14)) {
            let line = p.add(1);
            let h = u64::from_be((line.add(5) as *const u64).read_unaligned());
            if h >= target { raw[i + 1] = line as usize - bp as usize; break; }
            p = line;
            while p < bp.add(sz) && *p != b'\n' { p = p.add(1); }
        }
    }
    for i in 0..6 {
        (*s).b_start[i] = if i == 0 { 0 } else { raw[i].saturating_sub(margin) };
        (*s).b_end[i] = if i == 5 { sz } else { (raw[i + 1] + margin).min(sz) };
    }
}

fn main() {
    unsafe {
        mallopt(-8, 1);
        mallopt(-1, 0);
        let mut a = std::env::args().skip(1);
        let in_p = a.next().unwrap_or_else(|| "/data/requests.log".into());
        let out_p = a.next().unwrap_or_else(|| "/out/summary.txt".into());
        let (Ok(inf), Ok(outf)) = (File::open(&in_p), File::create(&out_p)) else { return; };
        let sz = inf.metadata().map(|m| m.len() as usize).unwrap_or(0);
        if sz > 0 {
            let mut base = mmap(0 as _, sz, 1, 1, inf.as_raw_fd(), 0) as usize;
            (base == !0).then(|| base = mmap(0 as _, sz, 1, 2, inf.as_raw_fd(), 0) as usize);

            compute_bounds(base as _, sz);
            let bar = Barrier::new(12);
            let (out_fd, bar) = (outf.as_raw_fd(), &bar);
            std::thread::scope(|sc| {
                for t in 1..12 {
                    Builder::new().stack_size(65536).spawn_scoped(sc, move || work(t, base as _, sz, out_fd, bar)).unwrap();
                }
                work(0, base as _, sz, out_fd, bar);
            });
        }
    }
}
