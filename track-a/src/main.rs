// Speed Demon 2026 — Track A (Ingest) Championship Rust Solution
#![allow(unsafe_op_in_unsafe_fn, dead_code, unused_unsafe, dangerous_implicit_autorefs)]
use std::arch::x86_64::{_mm256_loadu_si256 as ld, _mm_crc32_u64 as crc, *};
use std::ptr::copy_nonoverlapping as copy;
use std::slice::{from_raw_parts as slc, from_raw_parts_mut as slc_mut};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::*};
use std::{cell::UnsafeCell, fs::File, os::unix::io::AsRawFd, sync::Barrier, thread::Builder};
extern "C" {
    fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
    fn munmap(a: *mut u8, l: usize) -> i32;
    fn writev(fd: i32, iov: *const [usize; 2], cnt: i32) -> isize;
}
#[repr(transparent)] struct Global<T>(UnsafeCell<T>);
unsafe impl<T> Sync for Global<T> {}
#[repr(C, align(64))]
struct SharedData {
    iov: [[usize; 2]; 12], ep_c: [[u64; 2048]; 12],
    k: [AtomicU64; 33024], c: [AtomicU32; 163844], p: [usize; 2304],
    str_pool: [u8; 131072], str_off: AtomicU32,
    q: [u32; 13312 * 256], g: [u32; 65536], l: [[u32; 75000]; 12], n: [i32; 13312],
    h: [[[i32; 128]; 12]; 2], w: ([[u16; 128]; 12], [u16; 13312], [u16; 2304]),
    u: ([u8; 2304], [[u8; 163840]; 12]),
}
static S: Global<SharedData> = Global(UnsafeCell::new(unsafe { std::mem::zeroed() }));
static DIGITS: &[u8; 200] = b"00010203040506070809101112131415161718192021222324252627282930313233343536373839404142434445464748495051525354555657585960616263646566676869707172737475767778798081828384858687888990919293949596979899";
#[inline(always)]
unsafe fn fmt_u32_sp(p: *mut u8, mut v: u32) -> *mut u8 {
    let (mut b, mut i) = ([b' '; 12], 11usize);
    while v >= 100 {
        let q = v / 100;
        let r = (v - q * 100) as usize * 2;
        (v, i) = (q, i - 2);
        (b[i], b[i + 1]) = (DIGITS[r], DIGITS[r + 1]);
    }
    if v >= 10 {
        let r = v as usize * 2;
        i -= 2;
        (b[i], b[i + 1]) = (DIGITS[r], DIGITS[r + 1]);
    } else {
        i -= 1;
        b[i] = b'0' + v as u8;
    }
    copy(b.as_ptr().add(i), p, 12 - i);
    p.add(12 - i)
}
#[inline]
#[target_feature(enable = "avx2,sse4.2,bmi1,bmi2")]
unsafe fn intern(ci: usize, p: *const u8, len: u8, mut h: u64) -> u64 {
    let (s, mask) = (S.0.get(), if ci > 0 { 32767 } else { 255 });
    let c0 = crc(0, (p as *const u64).read_unaligned()) << 32;
    (ci == 0).then(|| h = ((c0 ^ h.wrapping_mul(0x9e3779b97f4a7c15)) & !0xFFFF) | (1 << 63));
    let (b_kv, b_id, mut idx) = ((1 - ci) << 15, (1 - ci) << 11, ((h >> 16) as usize) & mask);
    let mut kv = (*s).k[b_kv + idx].load(Acquire);
    while (kv ^ h) >= 65536 {
        if kv == 0 && (*s).k[b_kv + idx].compare_exchange(0, 1, AcqRel, Acquire).is_ok() {
            let j = b_id + (*s).c[163840 + ci].fetch_add(1, Relaxed) as usize;
            let off = (*s).str_off.fetch_add(len as u32, Relaxed) as usize;
            copy(p, (*s).str_pool.as_mut_ptr().add(off), len as usize);
            ((*s).p[j], (*s).u.0[j], (*s).w.2[j]) = (off, len, (j - b_id) as u16);
            (*s).k[b_kv + idx].store(h | ((j - b_id) as u64), Release);
        }
        idx = (idx + (kv > 1) as usize) & mask;
        kv = (*s).k[b_kv + idx].load(Acquire);
    }
    kv
}
#[target_feature(enable = "avx2,sse4.2,bmi1,bmi2")]
unsafe fn work(t: usize, bp: *const u8, sz: usize, out_fd: i32, bar: &Barrier) {
    let (s, lim, sv) = (S.0.get(), bp as usize + sz, _mm256_set1_epi8(b' ' as i8));
    let u8_at = |q: *const u8| (q as *const u64).read_unaligned();
    let find = |q: *const u8, b| slc(q, lim - q as usize).iter().position(|&c| c == b).unwrap();
    let sk = |i: usize| bp.add(if i > 0 && i < sz { i + find(bp.add(i - 1), b'\n') } else { i });
    let sp_m = |q: *const u8| _mm256_movemask_epi8(_mm256_cmpeq_epi8(ld(q as _), sv)) as u32 as u64;
    let (mut p, ec, mut lhw, mut hid) = (sk(t * sz / 12), sk((t + 1) * sz / 12), 0u64, 0usize);
    ((*s).h[0][t], (*s).h[1][t], (*s).w.0[t]) = ([-1; 128], [-1; 128], [256; 128]);
    let (mut cur_b, mut cur_f) = (-1i32, 256usize);
    let ep_c = &mut (*s).ep_c[t];
    while p < ec {
        let hw = u8_at(p.add(5));
        if hw != lhw {
            (cur_b >= 0).then(|| ((*s).h[1][t][hid], (*s).w.0[t][hid]) = (cur_b, cur_f as u16));
            (lhw, hid) = (hw, (intern(0, p, 13, hw) & 0xFFFF) as usize);
            (cur_b, cur_f) = ((*s).h[1][t][hid], (*s).w.0[t][hid] as usize);
        }
        let mut ep = p.add(32 + (*p.add(32) != b'/') as usize);
        while *ep != b'/' { ep = ep.add(1); }
        let m = if ep.add(64) <= ec { sp_m(ep) | (sp_m(ep.add(32)) << 32) } else { 0 };
        let el = if m != 0 { m.trailing_zeros() as usize } else { find(ep, b' ') };
        let (d, m8) = (el.saturating_sub(8), !0u64 >> ((8 - el.min(8)) * 8));
        let rd = |o: usize| u8_at(ep.add(o)) & m8;
        let (w0, w1, w2, w3) = (rd(0), rd(d >> 1), rd(d.saturating_sub(8)), rd(d));
        let h1 = crc(crc(crc(crc(el as u64, w0), w1), w2), w3);
        let h2 = (h1 << 32) | crc(crc(el as u64, w2 ^ w0), w3 ^ w1);
        let h = ((h2.wrapping_mul(0x9e3779b97f4a7c15) ^ (h1 << 16)) & !0xFFFF) | (1 << 63);
        let ci = ((h >> 16) as usize) & 2047;
        let mut kv = ep_c[ci];
        if (kv ^ h) >= 65536 {
            kv = intern(1, ep, el as u8, h);
            ep_c[ci] = kv;
        }
        let (eid, st_p) = ((kv & 0xFFFF) as usize, ep.add(el + 1));
        (*st_p == b'5').then(|| (*s).c[hid * 2048 + eid].fetch_add(1, Relaxed));
        let lp = st_p.add(4);
        let sub = if lp.add(8) <= ec { u8_at(lp).wrapping_sub(0x3030_3030_3030_3030) } else { 0 };
        let (mut c, mut lat) = (lp.add(1), (*lp - b'0') as u32);
        if sub & 0x8080_8080_8080_8080 != 0 {
            let tz = (sub & 0x8080_8080_8080_8080).trailing_zeros();
            let w = (((sub << 8) << (tz ^ 63)).wrapping_mul(2561) >> 8) & 0x00FF_00FF_00FF_00FF;
            let w2 = (w.wrapping_mul(6553601) >> 16 & 0xFFFF_0000_FFFF).wrapping_mul(42949672960001);
            (c, lat) = (lp.add((tz >> 3) as usize), (w2 >> 32) as u32);
        } else {
            while *c >= b'0' { (lat, c) = ((lat << 3) + (lat << 1) + (*c - b'0') as u32, c.add(1)); }
        }
        p = c.add(1 + (*c == b'
') as usize);
        if cur_f == 256 {
            (cur_b >= 0).then(|| (*s).w.1[cur_b as usize] = 256);
            let nb = (*s).c[163842].fetch_add(1, Relaxed) as i32;
            (*s).n[nb as usize] = -1;
            let link = if cur_b < 0 { &mut (*s).h[0][t][hid] } else { &mut (*s).n[cur_b as usize] };
            (*link, cur_b, cur_f) = (nb, nb, 0);
        }
        let mut hb = lat;
        if lat >= 1 << 20 {
            hb = 0x10_0000 | (*s).c[163843].fetch_add(1, Relaxed);
            (*s).g[(hb & 0xFFFF) as usize] = lat;
        }
        ((*s).q[cur_b as usize * 256 + cur_f], cur_f) = ((hb << 11) | eid as u32, cur_f + 1);
    }
    (cur_b >= 0).then(|| ((*s).h[1][t][hid], (*s).w.0[t][hid]) = (cur_b, cur_f as u16));
    for i in 0..128 { ((*s).h[1][t][i] >= 0).then(|| (*s).w.1[(*s).h[1][t][i] as usize] = (*s).w.0[t][i]); }
    bar.wait();
    if t == 0 { munmap(bp as *mut _, sz); }
    bar.wait();
    let (nh, ne) = ((*s).c[163840].load(Relaxed) as usize, (*s).c[163841].load(Relaxed) as usize);
    if t == 0 {
        let (p_arr, u0_arr, str_base) = ((*s).p.as_ptr(), (*s).u.0.as_ptr(), (*s).str_pool.as_ptr());
        let gs = |i: u16| slc(str_base.add(*p_arr.add(i as usize)), *u0_arr.add(i as usize) as _);
        (*s).w.2[2048..2048 + nh].sort_unstable_by(|&x, &y| gs(2048 + x).cmp(gs(2048 + y)));
        (*s).w.2[..ne].sort_unstable_by(|&x, &y| gs(x).cmp(gs(y)));
    }
    bar.wait();
    let (mut cnt, mut off, mut v_lats, mut bh) = ([0u32; 2048], [0u32; 2048], vec![], 0);
    let (g_ptr, str_base) = ((*s).g.as_ptr(), (*s).str_pool.as_ptr());
    let val = |v: u32| if (v as i32) < 0 { *g_ptr.add((v >> 11) as u16 as usize) } else { v >> 11 };
    while bh < nh {
        let (hi, out_b) = (bh + t, (*s).u.1[t].as_mut_ptr());
        let mut ptr = out_b;
        if hi < nh {
            let (h_id, mut tot) = ((*s).w.2[2048 + hi] as usize, 0u32);
            cnt[..ne].fill(0);
            for tr in 0..12 {
                let mut b = (*s).h[0][tr][h_id];
                while b >= 0 {
                    let bu = b as usize;
                    for &v in &(*s).q[bu * 256..bu * 256 + (*s).w.1[bu] as usize] { cnt[(v & 2047) as usize] += 1; }
                    b = (*s).n[bu];
                }
            }
            (0..ne).for_each(|i| (off[i], tot, cnt[i]) = (tot, tot + cnt[i], 0));
            let lp = if tot <= 75000 { (*s).l[t].as_mut_ptr() } else { v_lats.resize(tot as usize, 0); v_lats.as_mut_ptr() };
            for tr in 0..12 {
                let mut b = (*s).h[0][tr][h_id];
                while b >= 0 {
                    let bu = b as usize;
                    for &v in &(*s).q[bu * 256..bu * 256 + (*s).w.1[bu] as usize] {
                        let e = (v & 2047) as usize;
                        *lp.add((off[e] + cnt[e]) as usize) = val(v); cnt[e] += 1;
                    }
                    b = (*s).n[bu];
                }
            }
            for &eid_u in (*s).w.2[..ne].iter().filter(|&&e| cnt[e as usize] > 0) {
                let (eid, n) = (eid_u as usize, cnt[eid_u as usize] as usize);
                let l = slc_mut(lp.add(off[eid] as usize), n);
                let pct = |p: usize| (p * n + 99) / 100 - 1;
                let (i50, i95, i99) = (pct(50), pct(95), pct(99));
                (n <= 32).then(|| l.sort_unstable());
                (n > 32).then(|| l.select_nth_unstable(i50));
                (n > 32 && i95 > i50).then(|| l[i50 + 1..].select_nth_unstable(i95 - i50 - 1));
                (n > 32 && i99 > i95).then(|| l[i95 + 1..].select_nth_unstable(i99 - i95 - 1));
                let (err, el) = ((*s).c[h_id * 2048 + eid].load(Relaxed), (*s).u.0[eid] as usize);
                copy(str_base.add((*s).p[2048 + h_id]), ptr, 13);
                copy(b":00:00Z ".as_ptr(), ptr.add(13), 8);
                copy(str_base.add((*s).p[eid]), ptr.add(21), el);
                *ptr.add(21 + el) = b' '; ptr = ptr.add(22 + el);
                let ppm = ((err as u64) * 1_000_000 / (n as u64)) as u32;
                for v in [n as u32, l[i50], l[i95], l[i99], err, ppm] { ptr = fmt_u32_sp(ptr, v); }
                *ptr.sub(1) = b'\n';
            }
        }
        (*s).iov[t] = [out_b as usize, ptr as usize - out_b as usize];
        bar.wait();
        if t == 0 {
            let cnt = (nh - bh).min(12) as i32;
            let (mut cur_iov, mut rem) = ((*s).iov.as_mut_ptr(), cnt);
            while rem > 0 {
                let n = writev(out_fd, cur_iov, rem);
                if n <= 0 { break; }
                let mut written = n as usize;
                while rem > 0 && written >= (*cur_iov)[1] { written -= (*cur_iov)[1]; cur_iov = cur_iov.add(1); rem -= 1; }
                if rem > 0 && written > 0 { (*cur_iov)[0] += written; (*cur_iov)[1] -= written; }
            }
        }
        bar.wait();
        bh += 12;
    }
}
fn main() {
    unsafe {
        let mut a = std::env::args().skip(1);
        let in_p = a.next().unwrap_or_else(|| "/data/requests.log".into());
        let out_p = a.next().unwrap_or_else(|| "/out/summary.txt".into());
        let (Ok(inf), Ok(outf)) = (File::open(&in_p), File::create(&out_p)) else { return; };
        let sz = inf.metadata().map(|m| m.len() as usize).unwrap_or(0);
        if sz > 0 {
            let mut base = mmap(0 as _, sz, 1, 1, inf.as_raw_fd(), 0) as usize;
            (base == !0).then(|| base = mmap(0 as _, sz, 1, 2, inf.as_raw_fd(), 0) as usize);
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
