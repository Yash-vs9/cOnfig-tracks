// Speed Demon 2026 — Track A (Ingest) Championship Rust Solution
#![allow(
    unsafe_op_in_unsafe_fn,
    dead_code,
    unused_unsafe,
    dangerous_implicit_autorefs
)]
use std::arch::x86_64::{
    _mm256_cmpeq_epi8 as cmpeq, _mm256_loadu_si256 as ld, _mm256_movemask_epi8 as movemask,
    _mm256_set1_epi8 as set1, _mm_crc32_u64 as crc,
};
use std::ptr::copy_nonoverlapping as copy;
use std::slice::{from_raw_parts as slc, from_raw_parts_mut as slc_mut};
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering::*};
use std::{cell::UnsafeCell, fs::File, os::unix::io::AsRawFd, sync::Barrier, thread::Builder};

extern "C" {
    fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
    fn madvise(a: *mut u8, l: usize, adv: i32) -> i32;
    fn memchr(s: *const u8, c: i32, n: usize) -> *const u8;
    fn writev(fd: i32, iov: *const [usize; 2], cnt: i32) -> isize;
    fn mallopt(p: i32, v: i32) -> i32;
}

#[repr(transparent)]
struct Global<T>(UnsafeCell<T>);
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
        copy(
            DIGITS.as_ptr().add((v * 2) as usize),
            b.as_mut_ptr().add(i),
            2,
        );
    }
    copy(b.as_ptr().add(i), p, 12 - i);
    p.add(12 - i)
}

#[inline(always)]
unsafe fn hour_to_idx(p: *const u8) -> usize {
    let m = (*p.add(6) == b'1') as usize;
    let d = ((*p.add(8) - b'0') * 10 + (*p.add(9) - b'0')) as usize;
    let h = ((*p.add(11) - b'0') * 10 + (*p.add(12) - b'0')) as usize;
    (if m != 0 { 1 + d } else { d - 30 }) * 24 + h - 18
}

static mut HR_PFX: [[u8; 21]; 72] = [[0; 21]; 72];
unsafe fn init_hr_pfx() {
    for i in 0..72 {
        let (m, d, h) = if i < 6 {
            (10, 30, 18 + i)
        } else if i < 30 {
            (10, 31, i - 6)
        } else if i < 54 {
            (11, 1, i - 30)
        } else {
            (11, 2, i - 54)
        };
        let p = HR_PFX[i].as_mut_ptr();
        copy(b"2026-00-00T00:00:00Z ".as_ptr(), p, 21);
        copy(DIGITS.as_ptr().add(m * 2), p.add(5), 2);
        copy(DIGITS.as_ptr().add(d * 2), p.add(8), 2);
        copy(DIGITS.as_ptr().add(h * 2), p.add(11), 2);
    }
}

#[repr(C, align(64))]
struct SharedData {
    k: [AtomicU64; 32768],
    ep_count: AtomicU32,
    p: [usize; 2048],
    u0: [u8; 2048],
    ep_sort: [u16; 2048],
    q: [u32; 13824 * 256],
    g: [u32; 98304],
    next_g: AtomicU32,
    overflow_blk: AtomicI32,
    n: [i32; 13824],
    h: [[[i32; 72]; 12]; 2],
    w: [u16; 13824],
    out_len: [usize; 12],
    lat_slab: [[u32; 75000]; 12],
    out_buf: [[u8; 163840]; 12],
}
static S: Global<SharedData> = Global(UnsafeCell::new(unsafe { std::mem::zeroed() }));

#[inline(never)]
#[target_feature(enable = "avx2,sse4.2,bmi1,bmi2")]
unsafe fn intern(p: *const u8, len: u8, h: u64) -> u64 {
    let s = S.0.get();
    let mask = 32767;
    let mut idx = ((h >> 16) as usize) & mask;
    loop {
        let slot = &(*s).k[idx];
        let kv = slot.load(Relaxed);
        if (kv ^ h) < 65536 {
            let id = (kv & 0xFFFF) as usize;
            if (*s).u0[id] == len
                && slc((*s).p[id] as *const u8, len as usize) == slc(p, len as usize)
            {
                return kv;
            }
        }
        if kv == 0 && slot.compare_exchange(0, 1, AcqRel, Acquire).is_ok() {
            let id = (*s).ep_count.fetch_add(1, Relaxed) as usize;
            (*s).p[id] = p as usize;
            (*s).u0[id] = len;
            (*s).ep_sort[id] = id as u16;
            let full_kv = h | (id as u64);
            slot.store(full_kv, Release);
            return full_kv;
        }
        idx = (idx + (kv > 1) as usize) & mask;
    }
}

#[target_feature(enable = "avx2,sse4.2,bmi1,bmi2")]
unsafe fn work(t: usize, bp: *const u8, sz: usize, out_fd: i32, bar: &Barrier) {
    let (s, lim, sv) = (S.0.get(), bp as usize + sz, set1(b' ' as i8));
    let u8_at = |q: *const u8| (q as *const u64).read_unaligned();
    let find = |q: *const u8, b: u8| memchr(q, b as _, lim - q as usize) as usize - q as usize;
    let sk = |i: usize| {
        bp.add(if i > 0 && i < sz {
            i + find(bp.add(i - 1), b'\n')
        } else {
            i
        })
    };
    let sp_m = |q: *const u8| movemask(cmpeq(ld(q as _), sv)) as u32 as u64;

    let (mut p, ec) = (sk(t * sz / 12), sk((t + 1) * sz / 12));
    ((*s).h[0][t], (*s).h[1][t]) = ([-1; 72], [-1; 72]);
    let (mut cur_hid, mut cur_b, mut cur_f) = (usize::MAX, -1i32, 256usize);
    let mut next_blk = t * 1152;

    while p < ec {
        let hid = hour_to_idx(p);
        if hid != cur_hid {
            if cur_b >= 0 {
                (*s).h[1][t][cur_hid] = cur_b;
                (*s).w[cur_b as usize] = cur_f as u16;
            }
            cur_hid = hid;
            cur_b = (*s).h[1][t][cur_hid];
            cur_f = if cur_b >= 0 {
                (*s).w[cur_b as usize] as usize
            } else {
                256
            };
        }

        let x = u8_at(p.add(31)) ^ 0x2F2F_2F2F_2F2F_2F2F;
        let ep = p.add(
            31 + ((x.wrapping_sub(0x0101_0101_0101_0101) & !x & 0x8080_8080_8080_8080)
                .trailing_zeros()
                >> 3) as usize,
        );
        let m0 = sp_m(ep);
        let el = if m0 != 0 {
            m0.trailing_zeros() as usize
        } else {
            let m1 = sp_m(ep.add(32));
            if m1 != 0 {
                32 + m1.trailing_zeros() as usize
            } else {
                let m2 = sp_m(ep.add(64)) | (sp_m(ep.add(96)) << 32);
                64 + m2.trailing_zeros() as usize
            }
        };

        let (d, m8) = (el.saturating_sub(8), !0u64 >> ((8 - el.min(8)) * 8));
        let rd = |o: usize| u8_at(ep.add(o)) & m8;
        let (w0, w1, w2, w3) = (rd(0), rd(d >> 1), rd(d.saturating_sub(8)), rd(d));
        let c1 = crc(crc(el as u64, w0), w2);
        let c2 = crc(crc(el as u64 ^ 0x811c9dc5, w1), w3);
        let h = ((c1 << 32) | c2).wrapping_mul(0x9e3779b97f4a7c15);
        let h = ((h ^ (h >> 29)) & !0xFFFF) | (1 << 63);

        let mut kv = (*s).k[((h >> 16) as usize) & 32767].load(Relaxed);
        if (kv ^ h) >= 65536 {
            kv = intern(ep, el as u8, h);
        }
        let (eid, is_5xx) = ((kv & 0xFFFF) as usize, (*ep.add(el + 1) == b'5') as u32);

        let lp = ep.add(el + 5);
        let sub = u8_at(lp).wrapping_sub(0x3030_3030_3030_3030);
        let mask = sub & 0x8080_8080_8080_8080;
        let (c, lat) = if mask != 0 {
            let tz = mask.trailing_zeros();
            let w = (((sub << 8) << (tz ^ 63)).wrapping_mul(2561) >> 8) & 0x00FF_00FF_00FF_00FF;
            let m64 = (w.wrapping_mul(6553601) >> 16) & 0xFFFF_0000_FFFF;
            (
                lp.add((tz >> 3) as usize),
                (m64.wrapping_mul(0x2710_0000_0001) >> 32) as u32,
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
                (*s).w[cur_b as usize] = 256;
            }
            let nb = if next_blk < (t + 1) * 1152 {
                let b = next_blk as i32;
                next_blk += 1;
                b
            } else {
                (*s).overflow_blk.fetch_add(1, Relaxed)
            };
            (*s).n[nb as usize] = -1;
            if cur_b < 0 {
                (*s).h[0][t][cur_hid] = nb;
            } else {
                (*s).n[cur_b as usize] = nb;
            }
            (cur_b, cur_f) = (nb, 0);
        }

        let mut hb = lat;
        if lat >= 1 << 19 {
            let gi = (*s).next_g.fetch_add(1, Relaxed) as usize;
            hb = 0x8_0000 | (gi as u32);
            (*s).g[gi] = lat;
        }
        let v = (hb << 12) | (is_5xx << 11) | eid as u32;
        ((*s).q[cur_b as usize * 256 + cur_f], cur_f) = (v, cur_f + 1);
    }

    if cur_b >= 0 {
        (*s).h[1][t][cur_hid] = cur_b;
        (*s).w[cur_b as usize] = cur_f as u16;
    }

    bar.wait();

    let ne = (*s).ep_count.load(Relaxed) as usize;
    if t == 0 {
        let p_arr = (*s).p.as_ptr();
        let u0_arr = (*s).u0.as_ptr();
        let gs = |i: u16| {
            slc(
                *p_arr.add(i as usize) as *const u8,
                *u0_arr.add(i as usize) as usize,
            )
        };
        (*s).ep_sort[..ne].sort_unstable_by(|&x, &y| gs(x).cmp(gs(y)));
    }

    bar.wait();

    let (mut cnt, mut off, mut err_c, mut bh) = ([0u32; 2048], [0u32; 2048], [0u32; 2048], 0);
    let (g_ptr, lp) = ((*s).g.as_ptr(), (*s).lat_slab[t].as_mut_ptr());
    let val = |v: u32| {
        if (v as i32) < 0 {
            *g_ptr.add(((v >> 12) & 0x7_FFFF) as usize)
        } else {
            v >> 12
        }
    };

    while bh < 72 {
        let hid = bh + t;
        let out_b = (*s).out_buf[t].as_mut_ptr();
        let mut ptr = out_b;

        if hid < 72 {
            cnt[..ne].fill(0);
            err_c[..ne].fill(0);
            for tr in 0..12 {
                let mut b = (*s).h[0][tr][hid];
                while b >= 0 {
                    let bu = b as usize;
                    let w = (*s).w[bu] as usize;
                    let q_ptr = (*s).q.as_ptr().add(bu * 256);
                    for i in 0..w {
                        let v = *q_ptr.add(i);
                        let e = (v & 2047) as usize;
                        cnt[e] += 1;
                        err_c[e] += (v >> 11) & 1;
                    }
                    b = (*s).n[bu];
                }
            }

            let mut tot = 0u32;
            for i in 0..ne {
                off[i] = tot;
                tot += cnt[i];
                cnt[i] = 0;
            }

            for tr in 0..12 {
                let mut b = (*s).h[0][tr][hid];
                while b >= 0 {
                    let bu = b as usize;
                    let w = (*s).w[bu] as usize;
                    let q_ptr = (*s).q.as_ptr().add(bu * 256);
                    for i in 0..w {
                        let v = *q_ptr.add(i);
                        let e = (v & 2047) as usize;
                        *lp.add((off[e] + cnt[e]) as usize) = val(v);
                        cnt[e] += 1;
                    }
                    b = (*s).n[bu];
                }
            }

            for &eid_u in (*s).ep_sort[..ne].iter().filter(|&&e| cnt[e as usize] > 0) {
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

                let err = err_c[eid];
                let el = (*s).u0[eid] as usize;

                copy(HR_PFX[hid].as_ptr(), ptr, 21);
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

        (*s).out_len[t] = ptr as usize - out_b as usize;
        bar.wait();

        if t == 0 {
            let cur = (72 - bh).min(12);
            let mut iov = [[0usize; 2]; 12];
            for k in 0..cur {
                iov[k] = [(*s).out_buf[k].as_ptr() as usize, (*s).out_len[k]];
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
        init_hr_pfx();

        let s = S.0.get();
        (*s).overflow_blk.store(13824, Relaxed);
        madvise(s as *mut u8, std::mem::size_of::<SharedData>(), 14);

        let mut a = std::env::args().skip(1);
        let in_p = a.next().unwrap_or_else(|| "/data/requests.log".into());
        let out_p = a.next().unwrap_or_else(|| "/out/summary.txt".into());
        let (Ok(inf), Ok(outf)) = (File::open(&in_p), File::create(&out_p)) else {
            return;
        };
        let sz = inf.metadata().map(|m| m.len() as usize).unwrap_or(0);
        if sz > 0 {
            let mut base = mmap(0 as _, sz, 1, 1, inf.as_raw_fd(), 0) as usize;
            (base == !0).then(|| base = mmap(0 as _, sz, 1, 2, inf.as_raw_fd(), 0) as usize);
            madvise(base as *mut u8, sz, 2);
            let bar = Barrier::new(12);
            let (out_fd, b_ref) = (outf.as_raw_fd(), &bar);
            std::thread::scope(|sc| {
                for t in 1..12 {
                    Builder::new()
                        .stack_size(65536)
                        .spawn_scoped(sc, move || work(t, base as _, sz, out_fd, b_ref))
                        .unwrap();
                }
                work(0, base as _, sz, out_fd, b_ref);
            });
        }
    }
}
