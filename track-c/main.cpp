// Speed Demon 2026 — Track C (Aggregates in C++23) Championship Solution
#include <bits/stdc++.h>
#include <fcntl.h>
#include <immintrin.h>
#include <omp.h>
#include <sys/mman.h>
#include <sys/stat.h>

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
static size_t cs[13];

static inline void fmt_cents(char *&p, int64_t c) {
  c < 0 ? (*p++ = '-', c = -c) : 0;
  p = std::to_chars(p, p + 20, c / 100).ptr;
  *p++ = '.', *p++ = '0' + (c % 100) / 10, *p++ = '0' + (c % 10), *p++ = ' ';
}

static inline int64_t parse_u(const char *&p) {
  int64_t x = 0;
  while (*p != ',')
    x = (x << 3) + (x << 1) + (*p++ - '0');
  return ++p, x;
}

static inline uint64_t hash_bytes(const char *s, uint32_t len) {
  uint64_t h1 = len ^ 0x811c9dc5ULL, h2 = (uint64_t)len * 0x9e3779b97f4a7c15ULL,
           w = 0, w1 = 0;
  if (__builtin_expect(len - 8U <= 8U, 1)) {
    memcpy(&w, s, 8), memcpy(&w1, s + len - 8, 8);
    h1 = _mm_crc32_u64(_mm_crc32_u64(h1, w), w1);
    h2 = _mm_crc32_u64(_mm_crc32_u64(h2, w1), w);
    uint64_t h = (h1 ^ (h2 * 0x9e3779b97f4a7c15ULL));
    h ^= (h >> 31);
    return h ? h : 1;
  }
  uint32_t k = 0;
  for (; k + 8 <= len; k += 8) {
    memcpy(&w, s + k, 8);
    h1 = _mm_crc32_u64(h1, w), h2 = _mm_crc32_u64(h2, w);
  }
  if (k < len) {
    uint64_t tail = 0;
    for (uint32_t i = k; i < len; ++i)
      tail = (tail << 8) | (uint8_t)s[i];
    h1 = _mm_crc32_u64(h1, tail), h2 = _mm_crc32_u64(h2, tail);
  }
  uint64_t h = (h1 ^ (h2 * 0x9e3779b97f4a7c15ULL));
  h ^= (h >> 31);
  return h ? h : 1;
}

static inline bool is_rec_end(const char *base, size_t sz, const char *nl) {
  const char *p = nl;
  p > base &&p[-1] == '\r' ? --p : 0;
  if (p - base < 6 || p[-3] != '.' || p[-1] < '0' || p[-1] > '9' ||
      p[-2] < '0' || p[-2] > '9')
    return false;
  p -= 3;
  int d_am = 0, d_reg = 0, d_id = 0;
  while (p > base && p[-1] >= '0' && p[-1] <= '9')
    --p, ++d_am;
  if (!d_am || (p > base && p[-1] == '-' && --p, p <= base || p[-1] != ','))
    return false;
  --p;
  while (p > base && p[-1] >= '0' && p[-1] <= '9')
    --p, ++d_reg;
  if (!d_reg || p <= base || p[-1] != ',')
    return false;
  const char *nx = nl + 1;
  if (nx >= base + sz)
    return true;
  if (nx + 3 <= base + sz && (uint8_t)nx[0] == 0xEF && (uint8_t)nx[1] == 0xBB &&
      (uint8_t)nx[2] == 0xBF)
    nx += 3;
  while (nx < base + sz && *nx >= '0' && *nx <= '9')
    ++nx, ++d_id;
  return d_id > 0 && nx < base + sz && *nx == ',';
}

int main(int argc, char **argv) {
  int rfd = open(argc > 1 ? argv[1] : "/data/regions.csv", O_RDONLY);
  int mfd = open(argc > 2 ? argv[2] : "/data/records.csv", O_RDONLY);
  struct stat rst{}, mst{};
  fstat(rfd, &rst), fstat(mfd, &mst);
  size_t rsz = rst.st_size, sz = mst.st_size;
  const char *rb =
      rsz ? (const char *)mmap(0, rsz, PROT_READ, MAP_SHARED, rfd, 0) : nullptr;
  if (rsz && rb == MAP_FAILED)
    rb = (const char *)mmap(0, rsz, PROT_READ, MAP_PRIVATE, rfd, 0);
  if (rsz && rb != MAP_FAILED)
    madvise((void *)rb, rsz, MADV_WILLNEED | MADV_SEQUENTIAL);

  const char *base =
      sz ? (const char *)mmap(0, sz, PROT_READ, MAP_SHARED, mfd, 0) : nullptr;
  if (sz && base == MAP_FAILED)
    base = (const char *)mmap(0, sz, PROT_READ, MAP_PRIVATE, mfd, 0);
  if (sz && base != MAP_FAILED)
    madvise((void *)base, sz, MADV_WILLNEED | MADV_SEQUENTIAL);

  if (rsz) {
    for (const char *p = (const char *)memchr(rb, '\n', rsz) + 1;
         p < rb + rsz;) {
      while (*p++ != ',') {
      }
      c_to_dense[(p[0] - 'A') * 676 + (p[1] - 'A') * 26 + (p[2] - 'A') + 1] = 1;
      p += (p[3] == '\r' ? 5 : 4);
    }
  }
  uint8_t nc = 0;
  for (uint16_t c = 1; c <= 17576; ++c)
    if (c_to_dense[c]) {
      uint16_t v = c - 1;
      dense_to_code[nc][0] = 'A' + v / 676,
      dense_to_code[nc][1] = 'A' + (v / 26) % 26,
      dense_to_code[nc][2] = 'A' + v % 26;
      c_to_dense[c] = nc++;
    }
  if (rsz) {
    for (const char *p = (const char *)memchr(rb, '\n', rsz) + 1;
         p < rb + rsz;) {
      uint32_t reg = (uint32_t)parse_u(p);
      uint16_t code = (p[0] - 'A') * 676 + (p[1] - 'A') * 26 + (p[2] - 'A') + 1;
      if (reg < 1000000)
        reg_lut[reg] = (uint8_t)(c_to_dense[code] + 1);
      p += (p[3] == '\r' ? 5 : 4);
    }
  }
  uint8_t unm_cid = nc;

  size_t bo = sz ? ((const char *)memchr(base, '\n', sz) + 1 - base) : 0;
  for (int t = 0; t <= 12; ++t)
    cs[t] = bo + (sz - bo) * t / 12;

#pragma omp parallel num_threads(12)
  {
    int tid = omp_get_thread_num();
    if (tid > 0) {
      for (const char *p = base + cs[tid]; p < base + sz; ++p) {
        p = (const char *)memchr(p, '\n', (base + sz) - p);
        if (!p) {
          cs[tid] = sz;
          break;
        }
        if (is_rec_end(base, sz, p)) {
          cs[tid] = p - base + 1;
          break;
        }
      }
    }
#pragma omp barrier

    for (int i = 0; i < 256; ++i)
      T[tid][i].mi = INT64_MAX, T[tid][i].ma = INT64_MIN;
    const char *p = base + cs[tid], *end = base + cs[tid + 1];
    char tmp[520];
    alignas(64) uint64_t tl_cache[2048][2] = {0};
    const __m256i q_vec = _mm256_set1_epi8('"'), c_vec = _mm256_set1_epi8(',');

    auto find_q = [&](const char *s) -> const char * {
      if (__builtin_expect(s + 32 <= end, 1)) {
        uint32_t m = (uint32_t)_mm256_movemask_epi8(
            _mm256_cmpeq_epi8(_mm256_loadu_si256((const __m256i *)s), q_vec));
        if (m)
          return s + __builtin_ctz(m);
      }
      return (const char *)memchr(s, '"', end - s);
    };

    while (p < end) {
      if (__builtin_expect((uint8_t)*p == 0xEF, 0))
        p += 3;
      int64_t id = parse_u(p), nb = 0, ntb = 0, cents = 0;
      uint64_t nh = 0;

      if (__builtin_expect(*p == '"', 1)) {
        const char *q = find_q(++p);
        if (__builtin_expect(q[1] != '"', 1)) {
          nb = q - p, nh = hash_bytes(p, (uint32_t)nb), p = q + 2;
        } else {
          for (;;) {
            size_t seg = q - p + 1;
            if (nb + (int64_t)seg <= 512)
              memcpy(tmp + nb, p, seg);
            nb += seg, p = q + 2, q = find_q(p);
            if (q[1] != '"') {
              size_t rem = q - p;
              if (nb + (int64_t)rem <= 512)
                memcpy(tmp + nb, p, rem);
              nb += rem, p = q + 2;
              break;
            }
          }
          nh = hash_bytes(tmp, (uint32_t)std::min<int64_t>(nb, 512));
        }
      } else {
        const char *q = nullptr;
        if (__builtin_expect(p + 32 <= end, 1)) {
          uint32_t m = (uint32_t)_mm256_movemask_epi8(
              _mm256_cmpeq_epi8(_mm256_loadu_si256((const __m256i *)p), c_vec));
          if (m)
            q = p + __builtin_ctz(m);
        }
        if (!q)
          q = (const char *)memchr(p, ',', end - p);
        nb = q - p, nh = hash_bytes(p, (uint32_t)nb), p = q + 1;
      }

      if (__builtin_expect(*p == '"', 1)) {
        const char *s = ++p;
        while (p + 32 <= end) {
          uint32_t m = (uint32_t)_mm256_movemask_epi8(
              _mm256_cmpeq_epi8(_mm256_loadu_si256((const __m256i *)p), q_vec));
          while (m) {
            uint32_t idx = __builtin_ctz(m);
            if (__builtin_expect(p[idx + 1] != '"', 1)) {
              p += idx;
              goto done_q;
            }
            --ntb, m &= ~(3U << idx);
            if (idx == 31) {
              p += 33;
              goto next_vec;
            }
          }
          p += 32;
        next_vec:;
        }
        for (;; p += 2, --ntb) {
          p = (const char *)memchr(p, '"', end - p);
          if (p[1] != '"')
            break;
        }
      done_q:
        ntb += p - s, p += 2;
      } else {
        const char *s = p;
        while (p + 32 <= end) {
          uint32_t m = (uint32_t)_mm256_movemask_epi8(
              _mm256_cmpeq_epi8(_mm256_loadu_si256((const __m256i *)p), c_vec));
          if (m) {
            p += __builtin_ctz(m);
            goto found_c;
          }
          p += 32;
        }
        p = (const char *)memchr(p, ',', end - p);
      found_c:
        ntb += p - s, ++p;
      }

      uint32_t rid = (uint32_t)parse_u(p);
      uint8_t r = (rid < 1000000) ? reg_lut[rid] : 0;
      uint8_t cid = r ? (r - 1) : unm_cid;
      bool neg = (*p == '-') && ++p;
      while (*p != '.')
        cents = cents * 10 + (*p++ - '0');
      cents = (cents * 100 + p[1] * 10 + p[2] - 528) * (neg ? -1 : 1), p += 3;
      p += 1 + (*p == '\r');

      T[tid][cid].add({1, 0, id, cents, cents, cents, nb, ntb});

      uint32_t tl_idx = (uint32_t)nh & 2047;
      uint64_t tl_sig = (nh & ~0xFFULL) | cid;
      if (tl_cache[tl_idx][0] != tl_sig && tl_cache[tl_idx][1] != tl_sig) {
        tl_cache[tl_idx][1] = tl_cache[tl_idx][0];
        tl_cache[tl_idx][0] = tl_sig;
        for (uint32_t slot = (uint32_t)(nh ^ (nh >> 32)) & HT_MASK;;
             slot = (slot + 1) & HT_MASK) {
          uint64_t k = __atomic_load_n(&name_keys[slot], __ATOMIC_RELAXED);
          if (!k) {
            k = __sync_val_compare_and_swap(&name_keys[slot], 0ULL, nh);
            if (!k)
              k = nh;
          }
          if (k == nh) {
            uint64_t mask = 1ULL << (cid & 63), *w = &name_bits[slot][cid >> 6];
            if (!(*w & mask) &&
                !(__atomic_fetch_or(w, mask, __ATOMIC_RELAXED) & mask))
              T[tid][cid].dn++;
            break;
          }
        }
      }
    }
  }

  for (int cid = 0; cid <= unm_cid; ++cid)
    for (int tid = 0; tid < 12; ++tid) {
      G[cid].dn += T[tid][cid].dn;
      if (T[tid][cid].cnt)
        G[cid].add(T[tid][cid]);
    }

  char *ptr = out_buf;
  auto emit_u = [&](int64_t x) {
    ptr = std::to_chars(ptr, ptr + 24, x).ptr, *ptr++ = ' ';
  };
  for (int cid = 0; cid <= unm_cid; ++cid) {
    Agg &g = G[cid];
    if (!g.cnt)
      continue;
    if (cid == unm_cid)
      memcpy(ptr, "_UNMATCHED ", 11), ptr += 11;
    else
      memcpy(ptr, dense_to_code[cid], 3), ptr[3] = ' ', ptr += 4;
    emit_u(g.cnt), emit_u(g.id_s), fmt_cents(ptr, g.am_s), fmt_cents(ptr, g.mi),
        fmt_cents(ptr, g.ma);
    emit_u(g.dn), emit_u(g.nb), ptr = std::to_chars(ptr, ptr + 24, g.ntb).ptr,
                                *ptr++ = '\n';
  }
  int ofd = open(argc > 3 ? argv[3] : "/out/aggregates.txt",
                 O_WRONLY | O_CREAT | O_TRUNC, 0666);
  if (write(ofd, out_buf, ptr - out_buf) < 0) {
  }
  close(ofd);
  return 0;
}
