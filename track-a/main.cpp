#include <algorithm>
#include <cstdint>
#include <cstring>
#include <fcntl.h>
#include <immintrin.h>
#include <malloc.h>
#include <omp.h>
#include <string_view>
#include <sys/mman.h>
#include <sys/uio.h>
#include <unistd.h>

#pragma GCC optimize("O3,unroll-loops")
#pragma GCC target("avx2,bmi,bmi2,lzcnt,popcnt")

static inline int hour_to_idx(const char *p) {
    int m_bit = (p[6] == '1');
    int d = (p[8] - '0') * 10 + (p[9] - '0');
    int h = (p[11] - '0') * 10 + (p[12] - '0');
    return (m_bit ? 1 + d : d - 30) * 24 + h - 18;
}

static const char *DIGITS = "00010203040506070809101112131415161718192021222324252627282930313233343536373839404142434445464748495051525354555657585960616263646566676869707172737475767778798081828384858687888990919293949596979899";

static inline char* fmt_u32_sp(char *p, uint32_t v) {
    char b[12];
    int i = 11;
    b[11] = ' ';
    while (v >= 100) {
        int r = (v % 100) * 2;
        v /= 100;
        i -= 2;
        memcpy(b + i, DIGITS + r, 2);
    }
    if (v < 10) {
        b[--i] = '0' + v;
    } else {
        i -= 2;
        memcpy(b + i, DIGITS + v * 2, 2);
    }
    int len = 12 - i;
    memcpy(p, b + i, len);
    return p + len;
}

static char hr_pfx[72][22];
static void init_hr_pfx() {
    for (int i = 0; i < 72; ++i) {
        int m, day, hr;
        if (i < 6) { m = 10; day = 30; hr = 18 + i; }
        else if (i < 30) { m = 10; day = 31; hr = i - 6; }
        else if (i < 54) { m = 11; day = 1; hr = i - 30; }
        else { m = 11; day = 2; hr = i - 54; }
        char *p = hr_pfx[i];
        memcpy(p, "2026-00-00T00:00:00Z ", 21);
        p[5] = '0' + m / 10; p[6] = '0' + m % 10;
        p[8] = '0' + day / 10; p[9] = '0' + day % 10;
        p[11] = '0' + hr / 10; p[12] = '0' + hr % 10;
    }
}

struct alignas(64) Shared {
    uint64_t ep_ht[32768];
    uint32_t ep_count;
    uint32_t err_count[72 * 2048];
    const char *ep_ptr[2048];
    uint8_t ep_len[2048];
    uint16_t ep_sort[2048];

    uint32_t q[12288 * 256];
    uint32_t g[65536];
    uint32_t next_g;
    int32_t n[12288];
    int32_t h[12][72];
    int32_t cur_b[12][72];
    uint16_t cur_f[12][72];
    int32_t next_blk[12];
    uint64_t ep_cache[12][4096];

    char out_buf[12][163840];
    size_t out_len[12];
    uint32_t lat_slab[12][75000];
};

static Shared S;

static inline uint16_t intern_ep(const char *p, uint8_t len, uint64_t h) {
    uint32_t mask = 32767;
    uint32_t idx = ((h >> 16)) & mask;
    while (true) {
        uint64_t kv = __atomic_load_n(&S.ep_ht[idx], __ATOMIC_RELAXED);
        if ((kv ^ h) < 65536) {
            uint16_t id = (uint16_t)kv;
            if (S.ep_len[id] == len && memcmp(S.ep_ptr[id], p, len) == 0) {
                return id;
            }
        }
        if (kv == 0) {
            uint64_t expected = 0;
            if (__atomic_compare_exchange_n(&S.ep_ht[idx], &expected, 1, false, __ATOMIC_ACQ_REL, __ATOMIC_ACQUIRE)) {
                uint32_t id = __atomic_fetch_add(&S.ep_count, 1, __ATOMIC_RELAXED);
                S.ep_ptr[id] = p;
                S.ep_len[id] = len;
                S.ep_sort[id] = id;
                uint64_t full_kv = h | id;
                __atomic_store_n(&S.ep_ht[idx], full_kv, __ATOMIC_RELEASE);
                return id;
            }
            continue;
        }
        idx = (idx + (kv > 1)) & mask;
    }
}

int main(int argc, char **argv) {
    mallopt(-8, 1);
    mallopt(-1, 0);

    const char *in_p = argc > 1 ? argv[1] : "/data/requests.log";
    const char *out_p = argc > 2 ? argv[2] : "/out/summary.txt";

    int fd = open(in_p, O_RDONLY);
    if (fd < 0) return 0;
    size_t sz = lseek(fd, 0, SEEK_END);
    if (!sz) return 0;

    const char *base = (const char *)mmap(0, sz, PROT_READ, MAP_SHARED, fd, 0);
    madvise((void *)base, sz, MADV_SEQUENTIAL);
    int out_fd = open(out_p, O_WRONLY | O_CREAT | O_TRUNC, 0644);

    init_hr_pfx();
    memset(S.h, -1, sizeof(S.h));
    memset(S.cur_b, -1, sizeof(S.cur_b));
    for (int t = 0; t < 12; ++t) {
        S.next_blk[t] = t * 1024;
    }

    __m256i space_vec = _mm256_set1_epi8(' ');

    #pragma omp parallel num_threads(12)
    {
        int t = omp_get_thread_num();
        size_t s = t * sz / 12, e = (t + 1) * sz / 12;
        while (s && s < sz && base[s - 1] != '\n') ++s;
        while (e && e < sz && base[e - 1] != '\n') ++e;

        const char *p = base + s, *end = base + e;
        uint64_t *ep_cache = S.ep_cache[t];

        while (p < end) {
            int hid = hour_to_idx(p);

            // 1. SWAR find '/'
            uint64_t w = *(const uint64_t *)(p + 31) ^ 0x2F2F2F2F2F2F2F2FULL;
            uint64_t m = (w - 0x0101010101010101ULL) & ~w & 0x8080808080808080ULL;
            const char *ep = (p + 31) + (__builtin_ctzll(m) >> 3);

            // 2. AVX2 find endpoint end
            __m256i v0 = _mm256_loadu_si256((const __m256i *)ep);
            uint32_t sp_mask = _mm256_movemask_epi8(_mm256_cmpeq_epi8(v0, space_vec));
            uint32_t el = sp_mask ? __builtin_ctz(sp_mask) : 32;
            if (__builtin_expect(sp_mask == 0, 0)) {
                __m256i v1 = _mm256_loadu_si256((const __m256i *)(ep + 32));
                uint32_t m1 = _mm256_movemask_epi8(_mm256_cmpeq_epi8(v1, space_vec));
                if (m1) {
                    el += __builtin_ctz(m1);
                } else {
                    const char *cur = ep + 64;
                    while (*cur != ' ') ++cur;
                    el = cur - ep;
                }
            }

            // 3. CRC endpoint hash
            uint64_t w0 = 0, w1 = 0;
            memcpy(&w0, ep, std::min(8, (int)el));
            memcpy(&w1, ep + el - std::min(8, (int)el), std::min(8, (int)el));
            uint64_t h1 = _mm_crc32_u64(el, w0);
            uint64_t h2 = _mm_crc32_u64(h1, w1);
            uint64_t h = (((h2 * 0x9e3779b97f4a7c15ULL) ^ (h1 << 16)) & ~0xFFFFULL) | (1ULL << 63);

            uint32_t ci = ((h >> 16)) & 4095;
            uint64_t kv = ep_cache[ci];
            if ((kv ^ h) >= 65536) {
                kv = intern_ep(ep, el, h);
                ep_cache[ci] = kv;
            }
            uint16_t eid = kv & 0xFFFF;

            // 4. Status
            const char *st_p = ep + el + 1;
            if (*st_p == '5') {
                __atomic_fetch_add(&S.err_count[hid * 2048 + eid], 1, __ATOMIC_RELAXED);
            }

            // 5. SWAR latency parser
            const char *lp = st_p + 4;
            uint64_t val = *(const uint64_t *)lp;
            uint64_t sub = val - 0x3030303030303030ULL;
            uint64_t s_mask = sub & 0x8080808080808080ULL;
            uint32_t lat;
            const char *next_p;
            if (__builtin_expect(s_mask != 0, 1)) {
                int tz = __builtin_ctzll(s_mask);
                uint64_t w_dig = (((sub << 8) << (tz ^ 63)) * 2561ULL >> 8) & 0x00FF00FF00FF00FFULL;
                uint64_t m64 = (w_dig * 6553601ULL >> 16) & 0xFFFF0000FFFFULL;
                lat = (m64 * 42949672960001ULL) >> 32;
                next_p = lp + (tz >> 3) + 1;
            } else {
                lat = *lp++ - '0';
                while (*lp >= '0') lat = lat * 10 + (*lp++ - '0');
                next_p = lp + 1;
            }
            p = next_p;

            // 6. Block append
            int cb = S.cur_b[t][hid];
            uint16_t cf = S.cur_f[t][hid];
            if (cb < 0 || cf == 256) {
                int nb = S.next_blk[t]++;
                S.n[nb] = -1;
                if (cb < 0) {
                    S.h[t][hid] = nb;
                } else {
                    S.n[cb] = nb;
                }
                cb = nb;
                cf = 0;
                S.cur_b[t][hid] = cb;
            }
            uint32_t hb = lat;
            if (lat >= (1 << 20)) {
                int gi = __atomic_fetch_add(&S.next_g, 1, __ATOMIC_RELAXED);
                hb = (1 << 20) | gi;
                S.g[gi] = lat;
            }
            S.q[cb * 256 + cf] = (hb << 11) | eid;
            S.cur_f[t][hid] = cf + 1;
        }

        #pragma omp barrier

        if (t == 0) {
            uint32_t ne = S.ep_count;
            std::sort(S.ep_sort, S.ep_sort + ne, [](uint16_t a, uint16_t b) {
                std::string_view sa(S.ep_ptr[a], S.ep_len[a]);
                std::string_view sb(S.ep_ptr[b], S.ep_len[b]);
                return sa < sb;
            });
        }

        #pragma omp barrier

        uint32_t ne = S.ep_count;
        uint32_t cnt[2048] = {0};
        uint32_t off[2048] = {0};
        uint32_t *lp_slab = S.lat_slab[t];

        auto val_fn = [](uint32_t v) -> uint32_t {
            if ((int32_t)v < 0) {
                return S.g[(v >> 11) & 0xFFFF];
            } else {
                return v >> 11;
            }
        };

        for (int bh = 0; bh < 72; bh += 12) {
            int hid = bh + t;
            char *out_ptr = S.out_buf[t];

            if (hid < 72) {
                memset(cnt, 0, ne * sizeof(uint32_t));

                for (int tr = 0; tr < 12; ++tr) {
                    int b = S.h[tr][hid];
                    while (b >= 0) {
                        int w = (b == S.cur_b[tr][hid]) ? S.cur_f[tr][hid] : 256;
                        const uint32_t *q_ptr = S.q + b * 256;
                        for (int i = 0; i < w; ++i) {
                            cnt[q_ptr[i] & 2047]++;
                        }
                        b = S.n[b];
                    }
                }

                uint32_t tot = 0;
                for (uint32_t i = 0; i < ne; ++i) {
                    off[i] = tot;
                    tot += cnt[i];
                    cnt[i] = 0;
                }

                for (int tr = 0; tr < 12; ++tr) {
                    int b = S.h[tr][hid];
                    while (b >= 0) {
                        int w = (b == S.cur_b[tr][hid]) ? S.cur_f[tr][hid] : 256;
                        const uint32_t *q_ptr = S.q + b * 256;
                        for (int i = 0; i < w; ++i) {
                            uint32_t v = q_ptr[i];
                            uint32_t e = v & 2047;
                            lp_slab[off[e] + cnt[e]++] = val_fn(v);
                        }
                        b = S.n[b];
                    }
                }

                const char *hr_str = hr_pfx[hid];
                for (uint32_t k = 0; k < ne; ++k) {
                    uint16_t eid = S.ep_sort[k];
                    uint32_t n = cnt[eid];
                    if (!n) continue;

                    uint32_t *l = lp_slab + off[eid];
                    auto pct = [n](uint32_t p) { return (p * n + 99) / 100 - 1; };
                    uint32_t i50 = pct(50), i95 = pct(95), i99 = pct(99);

                    if (n <= 2) {
                        if (n == 2 && l[0] > l[1]) std::swap(l[0], l[1]);
                    } else if (n <= 32) {
                        std::sort(l, l + n);
                    } else {
                        std::nth_element(l, l + i50, l + n);
                        if (i95 > i50) std::nth_element(l + i50 + 1, l + i95, l + n);
                        if (i99 > i95) std::nth_element(l + i95 + 1, l + i99, l + n);
                    }

                    uint32_t err = S.err_count[hid * 2048 + eid];
                    uint8_t el = S.ep_len[eid];

                    memcpy(out_ptr, hr_str, 21);
                    out_ptr += 21;
                    memcpy(out_ptr, S.ep_ptr[eid], el);
                    out_ptr += el;
                    *out_ptr++ = ' ';

                    out_ptr = fmt_u32_sp(out_ptr, n);
                    out_ptr = fmt_u32_sp(out_ptr, l[i50]);
                    out_ptr = fmt_u32_sp(out_ptr, l[i95]);
                    out_ptr = fmt_u32_sp(out_ptr, l[i99]);

                    if (err == 0) {
                        memcpy(out_ptr, "0 0\n", 4);
                        out_ptr += 4;
                    } else {
                        out_ptr = fmt_u32_sp(out_ptr, err);
                        uint32_t ppm = (uint64_t)err * 1000000 / n;
                        out_ptr = fmt_u32_sp(out_ptr, ppm);
                        out_ptr[-1] = '\n';
                    }
                }
            }

            S.out_len[t] = out_ptr - S.out_buf[t];

            #pragma omp barrier

            if (t == 0) {
                int cur = std::min(12, 72 - bh);
                struct iovec iov[12];
                for (int k = 0; k < cur; ++k) {
                    iov[k].iov_base = S.out_buf[k];
                    iov[k].iov_len = S.out_len[k];
                }
                writev(out_fd, iov, cur);
            }

            #pragma omp barrier
        }
    }

    close(out_fd);
    return 0;
}
