#![allow(
    unsafe_op_in_unsafe_fn,
    dead_code,
    unused_unsafe,
    dangerous_implicit_autorefs
)]
use std::{
    arch::x86_64::{_mm256_loadu_si256 as ld, _mm_crc32_u64 as crc, *},
    cell::UnsafeCell,
    fs::File,
    os::unix::io::AsRawFd,
    ptr::copy_nonoverlapping as copy,
    slice::{from_raw_parts as slc, from_raw_parts_mut as slc_mut},
    sync::{
        atomic::{AtomicU32, AtomicU64, Ordering::*},
        Barrier,
    },
    thread::Builder,
};
extern "C" {
    fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
    fn memchr(s: *const u8, c: i32, n: usize) -> *const u8;
    fn writev(fd: i32, iov: *const [usize; 2], cnt: i32) -> isize;
    fn mallopt(p: i32, v: i32) -> i32;
    fn madvise(a: *mut u8, l: usize, adv: i32) -> i32;
}

#[repr(transparent)]
struct Global<T>(UnsafeCell<T>);
unsafe impl<T> Sync for Global<T> {}

static DIGITS: &[u8; 200] = b"00010203040506070809101112131415161718192021222324252627282930313233343536373839404142434445464748495051525354555657585960616263646566676869707172737475767778798081828384858687888990919293949596979899";

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
        copy(
            DIGITS.as_ptr().add((v * 2) as usize),
            b.as_mut_ptr().add(i),
            2,
        );
    }
    copy(b.as_ptr().add(i), p, 12 - i);
    p.add(12 - i)
}

#[repr(C, align(64))]
struct SharedData {
    k: [AtomicU64; 33024],
    c: [AtomicU32; 163842],
    p: [usize; 2304],
    q: [u32; 12288 * 256],
    g: [u32; 49152],
    l: [[u32; 75000]; 12],
    n: [i32; 12288],
    h: [[[i32; 128]; 12]; 2],
    w: ([u16; 12288], [u16; 2304]),
    u: ([u8; 2304], [[u8; 163840]; 12]),
}
static S: Global<SharedData> = Global(UnsafeCell::new(unsafe { std::mem::zeroed() }));

#[inline]
#[target_feature(enable = "avx2,sse4.2,bmi1,bmi2")]
unsafe fn intern(ci: usize, p: *const u8, len: u8, mut h: u64) -> u64 {
    let (s, mask) = (S.0.get(), if ci > 0 { 32767 } else { 255 });
    if ci == 0 {
        h = (((crc(0, (p as *const u64).read_unaligned()) << 32)
            ^ h.wrapping_mul(0x9e3779b97f4a7c15))
            & !0xFFFF)
            | (1 << 63);
    }
    let (b_kv, b_id, mut idx) = ((1 - ci) << 15, (1 - ci) << 11, ((h >> 16) as usize) & mask);
    loop {
        let slot = &(*s).k[b_kv + idx];
        let kv = slot.load(Relaxed);
        if (kv ^ h) < 65536 {
            return kv;
        }
        if kv == 0 && slot.compare_exchange(0, 1, AcqRel, Acquire).is_ok() {
            let j = b_id + (*s).c[163840 + ci].fetch_add(1, Relaxed) as usize;
            ((*s).p[j], (*s).u.0[j], (*s).w.1[j]) = (p as usize, len, (j - b_id) as u16);
            let full_kv = h | ((j - b_id) as u64);
            slot.store(full_kv, Release);
            return full_kv;
        }
        idx = (idx + (kv > 1) as usize) & mask;
    }
}

#[target_feature(enable = "avx2,sse4.2,bmi1,bmi2")]
unsafe fn work(t: usize, bp: *const u8, sz: usize, out_fd: i32, bar: &Barrier) {
    let (s, lim, sv) = (S.0.get(), bp as usize + sz, _mm256_set1_epi8(b' ' as i8));
    let u8_at = |q: *const u8| (q as *const u64).read_unaligned();
    let find = |q: *const u8, b: u8| memchr(q, b as _, lim - q as usize) as usize - q as usize;
    let sk = |i: usize| {
        bp.add(if i > 0 && i < sz {
            i + find(bp.add(i - 1), b'\n')
        } else {
            i
        })
    };
    let sp_m = |q: *const u8| _mm256_movemask_epi8(_mm256_cmpeq_epi8(ld(q as _), sv)) as u32 as u64;
    let (mut p, ec, mut lhw, mut hid) = (sk(t * sz / 12), sk((t + 1) * sz / 12), 0u64, 0usize);
    let (mut phw, mut phid) = (0u64, 0usize);
    ((*s).h[0][t], (*s).h[1][t]) = ([-1; 128], [-1; 128]);
    let (mut cur_b, mut cur_f, mut ep_c) = (-1i32, 256usize, [0u64; 8192]);
    let (mut next_blk, mut next_hi) = (t * 1024, t * 4096);

    while p < ec {
        let hw = u8_at(p.add(5));
        if hw != lhw {
            if cur_b >= 0 {
                (*s).h[1][t][hid] = cur_b;
                *(*s).w.0.get_unchecked_mut(cur_b as usize) = cur_f as u16;
            }
            let nhid = if hw == phw {
                phid
            } else {
                (intern(0, p, 13, hw) & 0xFFFF) as usize
            };
            (phw, phid, lhw, hid) = (lhw, hid, hw, nhid);
            cur_b = (*s).h[1][t][hid];
            cur_f = if cur_b >= 0 {
                *(*s).w.0.get_unchecked(cur_b as usize) as usize
            } else {
                256
            };
        }
        let x = u8_at(p.add(32)) ^ 0x2F2F_2F2F_2F2F_2F2F;
        let ep = p.add(
            32 + ((x.wrapping_sub(0x0101_0101_0101_0101) & !x & 0x8080_8080_8080_8080)
                .trailing_zeros()
                >> 3) as usize,
        );
        let m = sp_m(ep) | (sp_m(ep.add(32)) << 32);
        let el = if m != 0 {
            m.trailing_zeros() as usize
        } else {
            64 + (sp_m(ep.add(64)) | (sp_m(ep.add(96)) << 32)).trailing_zeros() as usize
        };
        let (d, m8) = (el.saturating_sub(8), !0u64 >> ((8 - el.min(8)) * 8));
        let rd = |o: usize| u8_at(ep.add(o)) & m8;
        let (w0, w1, w2, w3) = (rd(0), rd(d >> 1), rd(d.saturating_sub(8)), rd(d));
        let (a0, b0) = (crc(el as u64, w0), crc(el as u64, w2 ^ w0));
        let (a1, b1) = (crc(a0, w1), crc(b0, w3 ^ w1));
        let a2 = crc(a1, w2);
        let h1 = crc(a2, w3);
        let h2 = (h1 << 32) | b1;
        let h = ((h2.wrapping_mul(0x9e3779b97f4a7c15) ^ (h1 << 16)) & !0xFFFF) | (1 << 63);
        let ci = ((h >> 16) as usize) & 8191;
        let mut kv = ep_c[ci];
        if (kv ^ h) >= 65536 {
            kv = intern(1, ep, el as u8, h);
            ep_c[ci] = kv;
        }
        let eid = (kv & 0xFFFF) as usize;
        let st_p = ep.add(el + 1);
        if *st_p == b'5' {
            (*s).c.get_unchecked(hid * 2048 + eid).fetch_add(1, Relaxed);
        }
        let lp = st_p.add(4);
        let sub = u8_at(lp).wrapping_sub(0x3030_3030_3030_3030);
        let mask = sub & 0x8080_8080_8080_8080;
        let (c, lat) = if mask != 0 {
            let tz = mask.trailing_zeros();
            let w = (((sub << 8) << (tz ^ 63)).wrapping_mul(2561) >> 8) & 0x00FF_00FF_00FF_00FF;
            let m64 = (w.wrapping_mul(6553601) >> 16) & 0xFFFF_0000_FFFF;
            (
                lp.add((tz >> 3) as usize),
                (m64.wrapping_mul(42949672960001) >> 32) as u32,
            )
        } else {
            let (mut c, mut lat) = (lp.add(1), (*lp - b'0') as u32);
            while *c >= b'0' {
                (lat, c) = ((lat << 3) + (lat << 1) + (*c - b'0') as u32, c.add(1));
            }
            (c, lat)
        };
        p = c.add(1);
        if cur_f == 256 {
            if cur_b >= 0 {
                *(*s).w.0.get_unchecked_mut(cur_b as usize) = 256;
            }
            let nb = next_blk as i32;
            next_blk += 1;
            *(*s).n.get_unchecked_mut(nb as usize) = -1;
            if cur_b < 0 {
                (*s).h[0][t][hid] = nb;
            } else {
                *(*s).n.get_unchecked_mut(cur_b as usize) = nb;
            }
            (cur_b, cur_f) = (nb, 0);
        }
        let mut hb = lat;
        if lat >= 1 << 20 {
            let gi = next_hi;
            next_hi += 1;
            hb = 0x10_0000 | (gi as u32);
            *(*s).g.get_unchecked_mut(gi) = lat;
        }
        *(*s).q.get_unchecked_mut(cur_b as usize * 256 + cur_f) = (hb << 11) | eid as u32;
        cur_f += 1;
    }
    if cur_b >= 0 {
        (*s).h[1][t][hid] = cur_b;
        *(*s).w.0.get_unchecked_mut(cur_b as usize) = cur_f as u16;
    }
    bar.wait();
    let (nh, ne) = (
        (*s).c[163840].load(Relaxed) as usize,
        (*s).c[163841].load(Relaxed) as usize,
    );
    if t == 0 {
        let (p_arr, u0_arr) = ((*s).p.as_ptr(), (*s).u.0.as_ptr());
        let gs = |i: u16| {
            slc(
                *p_arr.add(i as usize) as *const u8,
                *u0_arr.add(i as usize) as usize,
            )
        };
        (*s).w.1[2048..2048 + nh].sort_unstable_by(|&x, &y| gs(2048 + x).cmp(gs(2048 + y)));
        (*s).w.1[..ne].sort_unstable_by(|&x, &y| gs(x).cmp(gs(y)));
    }
    bar.wait();
    let (mut cnt, mut off, mut bh) = ([0u32; 2048], [0u32; 2048], 0);
    let (g_ptr, lp) = ((*s).g.as_ptr(), (*s).l[t].as_mut_ptr());
    let val = |v: u32| {
        if (v as i32) < 0 {
            *g_ptr.add((v >> 11) as u16 as usize)
        } else {
            v >> 11
        }
    };
    while bh < nh {
        let hi = bh + t;
        let out_b = (*s).u.1[t].as_mut_ptr();
        let mut ptr = out_b;
        if hi < nh {
            let (h_id, mut tot) = ((*s).w.1[2048 + hi] as usize, 0u32);
            cnt[..ne].fill(0);
            for tr in 0..12 {
                let mut b = (*s).h[0][tr][h_id];
                while b >= 0 {
                    let bu = b as usize;
                    let w = *(*s).w.0.get_unchecked(bu) as usize;
                    let q_ptr = (*s).q.as_ptr().add(bu * 256);
                    for i in 0..w {
                        cnt[(*q_ptr.add(i) & 2047) as usize] += 1;
                    }
                    b = *(*s).n.get_unchecked(bu);
                }
            }
            (0..ne).for_each(|i| (off[i], tot, cnt[i]) = (tot, tot + cnt[i], 0));
            for tr in 0..12 {
                let mut b = (*s).h[0][tr][h_id];
                while b >= 0 {
                    let bu = b as usize;
                    let w = *(*s).w.0.get_unchecked(bu) as usize;
                    let q_ptr = (*s).q.as_ptr().add(bu * 256);
                    for i in 0..w {
                        let v = *q_ptr.add(i);
                        let e = (v & 2047) as usize;
                        *lp.add((off[e] + cnt[e]) as usize) = val(v);
                        cnt[e] += 1;
                    }
                    b = *(*s).n.get_unchecked(bu);
                }
            }
            let mut hr_pfx = [0u8; 21];
            copy((*s).p[2048 + h_id] as *const u8, hr_pfx.as_mut_ptr(), 13);
            copy(b":00:00Z ".as_ptr(), hr_pfx.as_mut_ptr().add(13), 8);
            for &eid_u in (*s).w.1[..ne].iter().filter(|&&e| cnt[e as usize] > 0) {
                let (eid, n) = (eid_u as usize, cnt[eid_u as usize] as usize);
                let l = slc_mut(lp.add(off[eid] as usize), n);
                let pct = |p| (p * n + 99) / 100 - 1;
                let (i50, i95, i99) = (pct(50), pct(95), pct(99));
                if n <= 2 {
                    if n == 2 && l[0] > l[1] {
                        l.swap(0, 1);
                    }
                } else if n <= 32 {
                    l.sort_unstable();
                } else {
                    l.select_nth_unstable(i50);
                    if i95 > i50 {
                        l[i50 + 1..].select_nth_unstable(i95 - i50 - 1);
                    }
                    if i99 > i95 {
                        l[i95 + 1..].select_nth_unstable(i99 - i95 - 1);
                    }
                }
                let (err, el) = (
                    (*s).c[h_id * 2048 + eid].load(Relaxed),
                    (*s).u.0[eid] as usize,
                );
                copy(hr_pfx.as_ptr(), ptr, 21);
                copy((*s).p[eid] as *const u8, ptr.add(21), el);
                *ptr.add(21 + el) = b' ';
                ptr = ptr.add(22 + el);
                for v in [n as u32, l[i50], l[i95], l[i99]] {
                    ptr = fmt_u32_sp(ptr, v);
                }
                if err == 0 {
                    *(ptr as *mut u32) = u32::from_ne_bytes(*b"0 0\n");
                    ptr = ptr.add(4);
                } else {
                    ptr = fmt_u32_sp(ptr, err);
                    ptr = fmt_u32_sp(ptr, ((err as u64) * 1_000_000 / (n as u64)) as u32);
                    *ptr.sub(1) = b'\n';
                }
            }
        }
        (*s).p[2176 + t] = ptr as usize - out_b as usize;
        bar.wait();
        if t == 0 {
            let cur = (nh - bh).min(12);
            let mut iov = [[0usize; 2]; 12];
            for k in 0..cur {
                iov[k] = [(*s).u.1[k].as_ptr() as usize, (*s).p[2176 + k]];
            }
            writev(out_fd, iov.as_ptr(), cur as i32);
        }
        bar.wait();
        bh += 12;
    }
}

fn main() {
    unsafe {
        mallopt(-8, 1);
        mallopt(-1, 0);
        let mut a = std::env::args().skip(1);
        let (in_p, out_p) = (
            a.next().unwrap_or_else(|| "/data/requests.log".into()),
            a.next().unwrap_or_else(|| "/out/summary.txt".into()),
        );
        let (Ok(inf), Ok(outf)) = (File::open(&in_p), File::create(&out_p)) else {
            return;
        };
        let sz = inf.metadata().map(|m| m.len() as usize).unwrap_or(0);
        if sz > 0 {
            let mut base = mmap(0 as _, sz, 1, 1, inf.as_raw_fd(), 0) as usize;
            (base == !0).then(|| base = mmap(0 as _, sz, 1, 2, inf.as_raw_fd(), 0) as usize);
            madvise(base as _, sz, 3);
            let bar = Barrier::new(12);
            let (out_fd, b_ref) = (outf.as_raw_fd(), &bar);
            std::thread::scope(|sc| {
                for t in 1..12 {
                    Builder::new()
                        .stack_size(262144)
                        .spawn_scoped(sc, move || work(t, base as _, sz, out_fd, b_ref))
                        .unwrap();
                }
                work(0, base as _, sz, out_fd, b_ref);
            });
        }
    }
}
