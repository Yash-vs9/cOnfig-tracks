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

struct Agg {
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
static Agg G[256], T[12][256];

constexpr uint32_t HT_CAP = 1 << 17, HT_MASK = HT_CAP - 1;
static uint64_t name_keys[HT_CAP], name_bits[HT_CAP][4];
static size_t cs[13], rcs[13];
static uint8_t unm_cid;

static inline void fmt_cents(char *&p, int64_t c) {
  c < 0 ? (*p++ = '-', c = -c) : 0;
  p = std::to_chars(p, p + 20, c / 100).ptr;
  *p++ = '.', *p++ = '0' + (c % 100) / 10, *p++ = '0' + (c % 10), *p++ = ' ';
}

static inline int64_t parse_u(const char *&p) {
  int64_t x = *p++ - '0';
  while (*p != ',') x = x * 10 + (*p++ - '0');
  return ++p, x;
}

static inline uint64_t hash_fast(const char *s, uint32_t len) {
  uint64_t h1 = len ^ 0x811c9dc5ULL, h2 = (uint64_t)len * 0x9e3779b97f4a7c15ULL;
  if (__builtin_expect(len <= 8, 0)) {
    uint64_t w = 0;
    if (len >= 4) {
      uint32_t a, b;
      memcpy(&a, s, 4), memcpy(&b, s + len - 4, 4);
      w = ((uint64_t)b << 32) | a;
    } else if (len > 0) {
      for (uint32_t i = 0; i < len; ++i) w = (w << 8) | (uint8_t)s[i];
    }
    h1 = _mm_crc32_u64(h1, w), h2 ^= w;
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
    for (uint32_t i = 0; i < len; ++i) h1 = _mm_crc32_u8((uint32_t)h1, (uint8_t)s[i]), h2 = (h2 * 31) + (uint8_t)s[i];
  }
  uint64_t h = (h1 ^ (h2 * 0x9e3779b97f4a7c15ULL));
  h ^= (h >> 31);
  return h ? h : 1;
}

static inline bool is_rec_end(const char *base, size_t sz, const char *nl) {
  const char *p = nl;
  p > base &&p[-1] == '\r' ? --p : 0;
  if (p - base < 6 || p[-3] != '.' || p[-1] < '0' || p[-1] > '9' || p[-2] < '0' || p[-2] > '9') return false;
  p -= 3;
  int d_am = 0, d_reg = 0, d_id = 0;
  while (p > base && p[-1] >= '0' && p[-1] <= '9') --p, ++d_am;
  if (!d_am || (p > base && p[-1] == '-' && --p, p <= base || p[-1] != ',')) return false;
  --p;
  while (p > base && p[-1] >= '0' && p[-1] <= '9') --p, ++d_reg;
  if (!d_reg || p <= base || p[-1] != ',') return false;
  const char *nx = nl + 1;
  if (nx >= base + sz) return true;
  if (nx + 3 <= base + sz && (uint8_t)nx[0] == 0xEF && (uint8_t)nx[1] == 0xBB && (uint8_t)nx[2] == 0xBF) nx += 3;
  while (nx < base + sz && *nx >= '0' && *nx <= '9') ++nx, ++d_id;
  return d_id > 0 && nx < base + sz && *nx == ',';
}

int main(int argc, char **argv) {
  int rfd = open(argc > 1 ? argv[1] : "/data/regions.csv", O_RDONLY);
  int mfd = open(argc > 2 ? argv[2] : "/data/records.csv", O_RDONLY);
  struct stat rst{}, mst{};
  fstat(rfd, &rst), fstat(mfd, &mst);
  size_t rsz = rst.st_size, sz = mst.st_size;
  const char *rb = rsz ? (const char *)mmap(0, rsz, PROT_READ, MAP_SHARED, rfd, 0) : nullptr;
  if (rsz && rb == MAP_FAILED) rb = (const char *)mmap(0, rsz, PROT_READ, MAP_PRIVATE, rfd, 0);
  if (rsz && rb != MAP_FAILED) madvise((void *)rb, rsz, MADV_WILLNEED | MADV_SEQUENTIAL);

  const char *base = sz ? (const char *)mmap(0, sz, PROT_READ, MAP_SHARED, mfd, 0) : nullptr;
  if (sz && base == MAP_FAILED) base = (const char *)mmap(0, sz, PROT_READ, MAP_PRIVATE, mfd, 0);
  if (sz && base != MAP_FAILED) madvise((void *)base, sz, MADV_WILLNEED | MADV_SEQUENTIAL);

  size_t ro = rsz ? ((const char *)memchr(rb, '\n', rsz) + 1 - rb) : 0;
  for (int t = 0; t <= 12; ++t) rcs[t] = ro + (rsz - ro) * t / 12;

  size_t bo = sz ? ((const char *)memchr(base, '\n', sz) + 1 - base) : 0;
  for (int t = 0; t <= 12; ++t) cs[t] = bo + (sz - bo) * t / 12;

#pragma omp parallel num_threads(12)
  {
    int tid = omp_get_thread_num();

    if (tid > 0 && rsz) {
      const char *p = (const char *)memchr(rb + rcs[tid], '\n', (rb + rsz) - (rb + rcs[tid]));
      rcs[tid] = p ? (p - rb + 1) : rsz;
    }

    if (tid > 0 && sz) {
      for (const char *p = base + cs[tid]; p < base + sz; ++p) {
        p = (const char *)memchr(p, '\n', (base + sz) - p);
        if (!p) { cs[tid] = sz; break; }
        if (is_rec_end(base, sz, p)) { cs[tid] = p - base + 1; break; }
      }
    }
#pragma omp barrier

    if (rsz) {
      const char *rp = rb + rcs[tid], *rend = rb + rcs[tid + 1];
      while (rp < rend) {
        while (*rp++ != ',') {}
        uint16_t code = (rp[0] - 'A') * 676 + (rp[1] - 'A') * 26 + (rp[2] - 'A') + 1;
        c_to_dense[code] = 1;
        rp += (rp[3] == '\r' ? 5 : 4);
      }
    }
#pragma omp barrier

    if (tid == 0) {
      uint8_t nc = 0;
      for (uint16_t c = 1; c <= 17576; ++c) {
        if (c_to_dense[c]) {
          uint16_t v = c - 1;
          dense_to_code[nc][0] = 'A' + v / 676, dense_to_code[nc][1] = 'A' + (v / 26) % 26, dense_to_code[nc][2] = 'A' + v % 26;
          c_to_dense[c] = nc++;
        }
      }
      unm_cid = nc;
    }
#pragma omp barrier

    if (rsz) {
      const char *rp = rb + rcs[tid], *rend = rb + rcs[tid + 1];
      while (rp < rend) {
        uint32_t reg = (uint32_t)parse_u(rp);
        uint16_t code = (rp[0] - 'A') * 676 + (rp[1] - 'A') * 26 + (rp[2] - 'A') + 1;
        if (reg < 1000000) reg_lut[reg] = (uint8_t)(c_to_dense[code] + 1);
        rp += (rp[3] == '\r' ? 5 : 4);
      }
    }
#pragma omp barrier

    for (int i = 0; i < 256; ++i) T[tid][i].mi = INT64_MAX, T[tid][i].ma = INT64_MIN;
    const char *p = base + cs[tid], *end = base + cs[tid + 1];
    char tmp[520];
    uint64_t tl[1024][2] = {};
    const __m256i q_vec = _mm256_set1_epi8('"'), c_vec = _mm256_set1_epi8(',');

    auto find_q = [&](const char *s) -> const char * {
      if (__builtin_expect(s + 32 <= end, 1)) {
        uint32_t m = (uint32_t)_mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256((const __m256i *)s), q_vec));
        if (m) return s + __builtin_ctz(m);
      }
      return (const char *)memchr(s, '"', end - s);
    };

    while (p < end) {
      if (__builtin_expect((uint8_t)*p == 0xEF, 0)) p += 3;
      int64_t id = parse_u(p), nb = 0, ntb = 0;
      uint64_t nh = 0;

      if (__builtin_expect(*p == '"', 1)) {
        const char *s = ++p;
        if (__builtin_expect(p + 32 <= end, 1)) {
          uint32_t m = (uint32_t)_mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256((const __m256i *)s), q_vec));
          uint32_t idx = __builtin_ctz(m);
          if (__builtin_expect(m && s[idx + 1] != '"', 1)) {
            nb = idx, nh = hash_fast(s, idx), p = s + idx + 2;
            goto name_done;
          }
        }
        {
          const char *q = find_q(p);
          if (__builtin_expect(q[1] != '"', 1)) {
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
        }
      } else {
        const char *q = nullptr;
        if (__builtin_expect(p + 32 <= end, 1)) {
          uint32_t m = (uint32_t)_mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256((const __m256i *)p), c_vec));
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
          uint32_t m = (uint32_t)_mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256((const __m256i *)s), q_vec));
          uint32_t idx = __builtin_ctz(m);
          if (__builtin_expect(m && s[idx + 1] != '"', 1)) {
            ntb = idx, p = s + idx + 2;
            goto note_done;
          }
        }
        while (p + 32 <= end) {
          uint32_t m = (uint32_t)_mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256((const __m256i *)p), q_vec));
          while (m) {
            uint32_t idx = __builtin_ctz(m);
            if (__builtin_expect(p[idx + 1] != '"', 1)) { p += idx; goto done_q; }
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
      } else {
        const char *s = p;
        while (p + 32 <= end) {
          uint32_t m = (uint32_t)_mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256((const __m256i *)p), c_vec));
          if (m) { p += __builtin_ctz(m); goto found_c; }
          p += 32;
        }
        p = (const char *)memchr(p, ',', end - p);
      found_c:
        ntb += p - s, ++p;
      }
    note_done:

      uint32_t rid = (uint32_t)parse_u(p);
      uint8_t r = (rid < 1000000) ? reg_lut[rid] : 0, cid = r ? (r - 1) : unm_cid;
      bool neg = (*p == '-') && ++p;
      int64_t cents = *p++ - '0';
      while (*p != '.') cents = cents * 10 + (*p++ - '0');
      cents = (cents * 100 + p[1] * 10 + p[2] - 528) * (neg ? -1 : 1), p += 3;
      p += 1 + (*p == '\r');

      T[tid][cid].add({1, 0, id, cents, cents, cents, nb, ntb});

      uint64_t sig = (nh & ~0xFFULL) | cid, *tc = tl[((uint32_t)(nh >> 8) ^ (cid * 131U)) & 1023];
      if (__builtin_expect(tc[0] != sig && tc[1] != sig, 0)) {
        tc[1] = tc[0], tc[0] = sig;
        uint32_t slot;
        for (slot = slot0;; slot = (slot + 1) & HT_MASK) {
          uint64_t k = name_keys[slot];
          if (__builtin_expect(k == nh, 1)) break;
          if (!k) {
            if (!__sync_bool_compare_and_swap(&name_keys[slot], 0ULL, nh) && name_keys[slot] != nh) continue;
            break;
          }
        }

        uint64_t mask = 1ULL << (cid & 63), *w = &name_bits[slot][cid >> 6];
        if (__builtin_expect(!(*w & mask), 1) && !(__atomic_fetch_or(w, mask, __ATOMIC_RELAXED) & mask)) T[tid][cid].dn++;
      }
    }
  }

  for (int cid = 0; cid <= unm_cid; ++cid)
    for (int tid = 0; tid < 12; ++tid) {
      G[cid].dn += T[tid][cid].dn;
      if (T[tid][cid].cnt) G[cid].add(T[tid][cid]);
    }

  char *ptr = out_buf;
  auto emit_u = [&](int64_t x) { ptr = std::to_chars(ptr, ptr + 24, x).ptr, *ptr++ = ' '; };
  for (int cid = 0; cid <= unm_cid; ++cid) {
    Agg &g = G[cid];
    if (!g.cnt) continue;
    if (cid == unm_cid) memcpy(ptr, "_UNMATCHED ", 11), ptr += 11;
    else memcpy(ptr, dense_to_code[cid], 3), ptr[3] = ' ', ptr += 4;
    emit_u(g.cnt), emit_u(g.id_s), fmt_cents(ptr, g.am_s), fmt_cents(ptr, g.mi), fmt_cents(ptr, g.ma);
    emit_u(g.dn), emit_u(g.nb), ptr = std::to_chars(ptr, ptr + 24, g.ntb).ptr, *ptr++ = '\n';
  }
  int ofd = open(argc > 3 ? argv[3] : "/out/aggregates.txt", O_WRONLY | O_CREAT | O_TRUNC, 0666);
  if (write(ofd, out_buf, ptr - out_buf) < 0) {}
  close(ofd);
  return 0;
}
