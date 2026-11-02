#include <algorithm>
#include <charconv>
#include <cstdint>
#include <cstring>
#include <fcntl.h>
#include <immintrin.h>
#include <omp.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

struct alignas(64) Agg {
  uint32_t cnt, dn;
  int64_t id_s, am_s, mi, ma, nb, ntb;
  void add(const Agg &t) {
    mi = cnt ? std::min(mi, t.mi) : t.mi, ma = cnt ? std::max(ma, t.ma) : t.ma;
    cnt += t.cnt, id_s += t.id_s, am_s += t.am_s, nb += t.nb, ntb += t.ntb;
  }
};

static uint8_t reg_lut[1000000];
static uint16_t c_to_dense[17577];
static char dense_to_code[256][4], out_buf[65536];
static Agg T[12][256];
constexpr uint32_t HT_CAP = 1 << 17, HT_MASK = HT_CAP - 1;
static uint64_t name_keys[HT_CAP], name_bits[HT_CAP][4];
static size_t cs[13];
static uint8_t unm_cid;

static inline uint32_t mm_find(const char *p, __m256i v) {
  return _mm256_movemask_epi8(_mm256_cmpeq_epi8(
      _mm256_loadu_si256((const __m256i *)p), v));
}
static inline void fmt_cents(char *&p, int64_t c) {
  c < 0 ? (*p++ = '-', c = -c) : 0;
  p = std::to_chars(p, p + 20, c / 100).ptr;
  *p++ = '.', *p++ = '0' + (c % 100) / 10, *p++ = '0' + (c % 10), *p++ = ' ';
}
static inline uint32_t parse_u(const char *&p) {
  uint32_t x = *p++ - '0';
  while (*p != ',') x = x * 10 + (*p++ - '0');
  return ++p, x;
}
static inline uint64_t hash_fast(const char *s, uint32_t len) {
  uint64_t h1 = len ^ 0x811c9dc5ULL, h2 = (uint64_t)len * 0x9e3779b97f4a7c15ULL;
  if (__builtin_expect(len <= 8, 1)) {
    uint64_t w = 0;
    memcpy(&w, s, 8);
    uint64_t m = _bzhi_u64(w, len << 3);
    h1 = _mm_crc32_u64(h1, m), h2 ^= m;
  } else if (__builtin_expect(len <= 16, 1)) {
    uint64_t a, b;
    memcpy(&a, s, 8), memcpy(&b, s + len - 8, 8);
    h1 = _mm_crc32_u64(h1, a), h2 = _mm_crc32_u64(h2, b);
  } else if (len <= 32) {
    uint64_t a, b, c, d;
    memcpy(&a, s, 8), memcpy(&b, s + 8, 8);
    memcpy(&c, s + len - 16, 8), memcpy(&d, s + len - 8, 8);
    h1 = _mm_crc32_u64(_mm_crc32_u64(h1, a), c);
    h2 = _mm_crc32_u64(_mm_crc32_u64(h2, b), d);
  } else {
    for (uint32_t i = 0; i < len; ++i)
      h1 = _mm_crc32_u8((uint32_t)h1, (uint8_t)s[i]),
      h2 = (h2 * 31) + (uint8_t)s[i];
  }
  uint64_t h = (h1 ^ (h2 * 0x9e3779b97f4a7c15ULL));
  return (h ^ (h >> 31)) ? (h ^ (h >> 31)) : 1;
}
static inline bool is_rec_end(const char *b, size_t sz, const char *nl) {
  const char *p = nl - (nl > b && nl[-1] == '\r');
  if (p - b < 6 || p[-3] != '.' || (uint32_t)(p[-1] - '0') > 9 ||
      (uint32_t)(p[-2] - '0') > 9)
    return 0;
  p -= 3;
  const char *s = p;
  while (p > b && (uint32_t)(p[-1] - '0') <= 9) --p;
  if (p == s || (p > b && p[-1] == '-' && --p, p <= b || p[-1] != ','))
    return 0;
  s = --p;
  while (p > b && (uint32_t)(p[-1] - '0') <= 9) --p;
  if (p == s || p <= b || p[-1] != ',') return 0;
  const char *nx = nl + 1;
  if (nx >= b + sz) return 1;
  bool bom = nx + 3 <= b + sz && (uint8_t)nx[0] == 0xEF &&
             (uint8_t)nx[1] == 0xBB && (uint8_t)nx[2] == 0xBF;
  nx += bom ? 3 : 0, s = nx;
  while (nx < b + sz && (uint32_t)(*nx - '0') <= 9) ++nx;
  return nx > s && nx < b + sz && *nx == ',';
}

int main(int argc, char **argv) {
  int rfd = open(argc > 1 ? argv[1] : "/data/regions.csv", O_RDONLY);
  int mfd = open(argc > 2 ? argv[2] : "/data/records.csv", O_RDONLY);
  struct stat rst{}, mst{};
  fstat(rfd, &rst), fstat(mfd, &mst);
  size_t rsz = rst.st_size, sz = mst.st_size;
  auto map_f = [&](int fd, size_t len) -> const char * {
    if (!len) return nullptr;
    const char *b = (const char *)mmap(0, len, PROT_READ, MAP_SHARED, fd, 0);
    if (b == MAP_FAILED)
      b = (const char *)mmap(0, len, PROT_READ, MAP_PRIVATE, fd, 0);
    if (b != MAP_FAILED) madvise((void *)b, len, MADV_WILLNEED | MADV_SEQUENTIAL);
    return b;
  };
  const char *rb = map_f(rfd, rsz), *base = map_f(mfd, sz);

  if (rsz) {
    const char *rp = (const char *)memchr(rb, '\n', rsz) + 1, *rend = rb + rsz,
               *rstart = rp;
    while (rp < rend) {
      while (*rp++ != ',') {}
      uint16_t cd = (rp[0] - 'A') * 676 + (rp[1] - 'A') * 26 + rp[2] - 'A' + 1;
      c_to_dense[cd] = 1, rp += (rp[3] == '\r' ? 5 : 4);
    }
    uint8_t nc = 0;
    for (uint16_t c = 1; c <= 17576; ++c) {
      if (c_to_dense[c]) {
        uint16_t v = c - 1;
        char *d = dense_to_code[nc];
        d[0] = 'A' + v / 676, d[1] = 'A' + (v / 26) % 26, d[2] = 'A' + v % 26;
        c_to_dense[c] = nc++;
      }
    }
    unm_cid = nc, rp = rstart;
    while (rp < rend) {
      uint32_t reg = parse_u(rp);
      uint16_t cd = (rp[0] - 'A') * 676 + (rp[1] - 'A') * 26 + rp[2] - 'A' + 1;
      if (reg < 1000000) reg_lut[reg] = (uint8_t)(c_to_dense[cd] + 1);
      rp += (rp[3] == '\r' ? 5 : 4);
    }
  }

  size_t bo = sz ? ((const char *)memchr(base, '\n', sz) + 1 - base) : 0;
  for (int t = 0; t <= 12; ++t) cs[t] = bo + (sz - bo) * t / 12;

#pragma omp parallel num_threads(12)
  {
    int tid = omp_get_thread_num();
    if (tid > 0 && sz) {
      for (const char *p = base + cs[tid]; p < base + sz; ++p) {
        p = (const char *)memchr(p, '\n', (base + sz) - p);
        if (!p) { cs[tid] = sz; break; }
        if (is_rec_end(base, sz, p)) { cs[tid] = p - base + 1; break; }
      }
    }
#pragma omp barrier

    for (int i = 0; i < 256; ++i)
      T[tid][i].mi = INT64_MAX, T[tid][i].ma = INT64_MIN;
    const char *p = base + cs[tid], *end = base + cs[tid + 1];
    char tmp[520];
    const __m256i q_vec = _mm256_set1_epi8('"'), c_vec = _mm256_set1_epi8(',');

    auto find_q = [&](const char *s) -> const char * {
      if (__builtin_expect(s + 32 <= end, 1)) {
        uint32_t m = mm_find(s, q_vec);
        if (m) return s + __builtin_ctz(m);
      }
      return (const char *)memchr(s, '"', end - s);
    };

    while (p < end) {
      if (__builtin_expect((uint8_t)*p == 0xEF, 0)) p += 3;
      uint32_t id = parse_u(p);
      int64_t nb = 0, ntb = 0;
      uint64_t nh = 0;

      if (__builtin_expect(*p == '"', 1)) {
        const char *s = ++p;
        uint32_t m = 0, idx = 0;
        if (__builtin_expect(p + 32 <= end, 1)) {
          m = mm_find(s, q_vec);
          if (__builtin_expect(m != 0, 1)) {
            idx = __builtin_ctz(m);
            if (__builtin_expect(s[idx + 1] != '"', 1)) {
              nb = idx, nh = hash_fast(s, idx), p = s + idx + 2;
              goto name_done;
            }
          }
        }
        const char *q = m ? (s + idx) : find_q(p);
        if (q[1] != '"') {
          nb = q - p, nh = hash_fast(p, (uint32_t)nb), p = q + 2;
        } else {
          for (;;) {
            size_t seg = q - p + 1;
            if (nb + (int64_t)seg <= 512) memcpy(tmp + nb, p, seg);
            nb += seg, p = q + 2, q = find_q(p);
            if (q[1] != '"') {
              size_t rem = q - p;
              if (nb + (int64_t)rem <= 512) memcpy(tmp + nb, p, rem);
              nb += rem, p = q + 2;
              break;
            }
          }
          nh = hash_fast(tmp, (uint32_t)nb);
        }
      } else if (__builtin_expect(*p == ',', 0)) {
        nb = 0, nh = 3902269347ULL, ++p;
      } else {
        const char *q = nullptr;
        if (__builtin_expect(p + 32 <= end, 1)) {
          uint32_t m = mm_find(p, c_vec);
          if (m) q = p + __builtin_ctz(m);
        }
        if (!q) q = (const char *)memchr(p, ',', end - p);
        nb = q - p, nh = hash_fast(p, (uint32_t)nb), p = q + 1;
      }
    name_done:
      uint32_t slot0 = (uint32_t)(nh ^ (nh >> 32)) & HT_MASK;
      _mm_prefetch((const char *)&name_keys[slot0], _MM_HINT_T0);
      _mm_prefetch((const char *)&name_bits[slot0], _MM_HINT_T0);

      if (__builtin_expect(*p == '"', 1)) {
        const char *s = ++p;
        if (__builtin_expect(p + 32 <= end, 1)) {
          uint32_t m = mm_find(s, q_vec);
          if (m) {
            uint32_t idx = __builtin_ctz(m);
            if (__builtin_expect(s[idx + 1] != '"', 1)) {
              ntb = idx, p = s + idx + 2;
              goto note_done;
            }
          } else if (__builtin_expect(p + 64 <= end, 1)) {
            uint32_t m2 = mm_find(s + 32, q_vec);
            if (m2) {
              uint32_t idx2 = __builtin_ctz(m2);
              if (__builtin_expect(s[32 + idx2 + 1] != '"', 1)) {
                ntb = 32 + idx2, p = s + 32 + idx2 + 2;
                goto note_done;
              }
            }
            p = s + 32;
          }
        }
        while (p + 32 <= end) {
          uint32_t m = mm_find(p, q_vec);
          while (m) {
            uint32_t idx = __builtin_ctz(m);
            if (__builtin_expect(p[idx + 1] != '"', 1)) {
              p += idx;
              goto done_q;
            }
            --ntb, m &= ~(3U << idx);
            if (idx == 31) { p += 33; goto next_vec; }
          }
          p += 32;
        next_vec:;
        }
        for (;; p += 2, --ntb) {
          p = (const char *)memchr(p, '"', end - p);
          if (p[1] != '"') break;
        }
      done_q:
        ntb += p - s, p += 2;
      } else if (__builtin_expect(*p == ',', 0)) {
        ntb = 0, ++p;
      } else {
        const char *s = p;
        if (__builtin_expect(p + 32 <= end, 1)) {
          uint32_t m = mm_find(s, c_vec);
          if (m) {
            ntb = __builtin_ctz(m), p = s + ntb + 1;
            goto note_done;
          }
          if (__builtin_expect(p + 64 <= end, 1)) {
            uint32_t m2 = mm_find(s + 32, c_vec);
            if (m2) {
              ntb = 32 + __builtin_ctz(m2), p = s + ntb + 1;
              goto note_done;
            }
            p = s + 64;
          } else
            p = s + 32;
        }
        p = (const char *)memchr(p, ',', end - p), ntb = p - s, ++p;
      }
    note_done:
      uint32_t rid = parse_u(p);
      uint8_t r = (rid < 1000000) ? reg_lut[rid] : 0, cid = r ? (r - 1) : unm_cid;
      bool neg = (*p == '-') && ++p;
      int64_t cents = *p++ - '0';
      if (*p != '.')
        while (*p != '.') cents = cents * 10 + (*p++ - '0');
      cents = cents * 100 + p[1] * 10 + p[2] - 528, p += 3;
      if (__builtin_expect(neg, 0)) cents = -cents;
      p += 1 + (*p == '\r');

      Agg &a = T[tid][cid];
      a.cnt++, a.id_s += id, a.am_s += cents, a.nb += nb, a.ntb += ntb;
      a.mi = std::min(a.mi, cents), a.ma = std::max(a.ma, cents);

      uint32_t slot;
      for (slot = slot0;; slot = (slot + 1) & HT_MASK) {
        uint64_t k = name_keys[slot];
        if (__builtin_expect(k == nh, 1)) break;
        if (!k) {
          if (!__sync_bool_compare_and_swap(&name_keys[slot], 0ULL, nh) &&
              name_keys[slot] != nh)
            continue;
          break;
        }
      }
      uint64_t mask = 1ULL << (cid & 63), *w = &name_bits[slot][cid >> 6];
      if (__builtin_expect(!(*w & mask), 1) &&
          !(__atomic_fetch_or(w, mask, __ATOMIC_RELAXED) & mask))
        a.dn++;
    }
  }

  for (int cid = 0; cid <= unm_cid; ++cid)
    for (int tid = 1; tid < 12; ++tid) {
      T[0][cid].dn += T[tid][cid].dn;
      if (T[tid][cid].cnt) T[0][cid].add(T[tid][cid]);
    }

  char *ptr = out_buf;
  auto emit_u = [&](int64_t x) {
    ptr = std::to_chars(ptr, ptr + 24, x).ptr, *ptr++ = ' ';
  };
  for (int cid = 0; cid <= unm_cid; ++cid) {
    Agg &g = T[0][cid];
    if (!g.cnt) continue;
    if (cid == unm_cid) memcpy(ptr, "_UNMATCHED ", 11), ptr += 11;
    else memcpy(ptr, dense_to_code[cid], 3), ptr[3] = ' ', ptr += 4;
    emit_u(g.cnt), emit_u(g.id_s), fmt_cents(ptr, g.am_s), fmt_cents(ptr, g.mi);
    fmt_cents(ptr, g.ma), emit_u(g.dn), emit_u(g.nb);
    ptr = std::to_chars(ptr, ptr + 24, g.ntb).ptr, *ptr++ = '\n';
  }
  int ofd = open(argc > 3 ? argv[3] : "/out/aggregates.txt",
                 O_WRONLY | O_CREAT | O_TRUNC, 0666);
  if (write(ofd, out_buf, ptr - out_buf) < 0) {}
  close(ofd);
  return 0;
}
