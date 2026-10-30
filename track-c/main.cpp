#include <bits/stdc++.h>
#include <fcntl.h>
#include <omp.h>
#include <sys/mman.h>

// High-speed decimal formatting directly into the pre-allocated output buffer
#define P(x) (o = std::to_chars(o, o + 24, x).ptr, *o++ = ' ')
// 64-bit FNV-1a prime multiply step for on-the-fly unescaped name hashing
#define H(c) (nh = (nh ^ (uint8_t)(c)) * 1099511628211ULL)
// Fast branchless quote unescaping: consumes "" pairs as 1 literal byte
#define C (*p == '"' ? (p += 2, '"') : *p++)

using V = std::vector<uint64_t>;

// Country aggregate statistics accumulator: O(1) lock-free per-thread tracking
struct Agg {
  uint32_t cnt, dn;
  int64_t id_s, am_s, mi, ma, nb, ntb;
  void add(const Agg &t) {
    mi = cnt ? std::min(mi, t.mi) : t.mi, ma = cnt ? std::max(ma, t.ma) : t.ma;
    cnt += t.cnt, id_s += t.id_s, am_s += t.am_s, nb += t.nb, ntb += t.ntb;
  }
};

// Zero-allocation BSS direct lookup tables and thread-sharded buffers
static uint16_t reg_lut[1000000];
static Agg G[17577], T[12][17577];
static V B[12][12];
static size_t cs[13], qc[12], iq[12], rsz, sz;
static char buf[1 << 20], *o = buf;

// Formats signed cents into decimal string without floating-point arithmetic
static void fc(int64_t c) {
  c < 0 ? *o++ = '-', c = -c : 0;
  o = std::to_chars(o, o + 20, c / 100).ptr;
  *o++ = '.', *o++ = '0' + c % 100 / 10, *o++ = '0' + c % 10, *o++ = ' ';
}

// Parses unquoted positive integer from comma-separated input
static int64_t gi(const char *&p, int64_t x = 0) {
  while (*p != ',')
    x = x * 10 + (*p++ - '0');
  return ++p, x;
}

int main(int c, char **v) {
  // Stage 1: Zero-copy mmap of regions and records files over pre-warmed OS
  // cache
  int r = open(c > 1 ? v[1] : "/data/regions.csv", 0),
      m = open(c > 2 ? v[2] : "/data/records.csv", 0);
  const char *rb = (char *)mmap(0, rsz = lseek(r, 0, 2), 1, 1, r, 0),
             *base = (char *)mmap(0, sz = lseek(m, 0, 2), 1, 1, m, 0);

  // O(1) BSS direct lookup table for 1M regions (0 = _UNMATCHED, 1..17576 =
  // AAA..ZZZ)
  for (const char *p = (char *)memchr(rb, '\n', rsz) + 1; p < rb + rsz;) {
    uint32_t reg = gi(p);
    reg_lut[reg] = p[0] * 676 + p[1] * 26 + p[2] - 45694;
    while (p < rb + rsz && *p++ != '\n')
      ;
  }

  size_t bo = (char *)memchr(base, '\n', sz) + 1 - base, q = 0;

  // Stage 2: Two-pass parallel quote-parity scan resolves exact record
  // boundaries across embedded LFs
#pragma omp parallel num_threads(12)
  {
    int tid = omp_get_thread_num();
    size_t s0 = bo + (sz - bo) * tid / 12, s1 = bo + (sz - bo) * (tid + 1) / 12;
    qc[tid] = std::count(base + s0, base + s1, '"');
#pragma omp barrier
#pragma omp single
    for (int t = (cs[0] = bo, cs[12] = sz, 0); t < 12; ++t)
      iq[t] = q, q ^= (qc[t] & 1);
    const char *p0 = base + s0;
    while (tid && p0 < base + sz && (*p0 != '\n' || iq[tid]))
      iq[tid] ^= (*p0++ == '"');
    tid ? cs[tid] = p0 < base + sz ? p0 - base + 1 : sz : 0;
#pragma omp barrier

    // Stage 3: Lock-free streaming CSV parser with on-the-fly FNV-1a name
    // hashing and BSS region join
    const char *p = base + cs[tid], *end = base + cs[tid + 1];
    bool q, neg;
    while (p < end) {
      p += (uint8_t)*p == 0xEF ? 3 : 0; // Strip optional mid-file UTF-8 BOM
      int64_t id = gi(p), nb = 0, ntb = 0, cents = 0;
      uint64_t nh = 14695981039346656037ULL;

      // Scan name and note fields handling quotes, commas, and embedded
      // newlines
      for (int f = 0; f < 2; ++f) {
        p += (q = *p == '"');
        while (q ? *p != '"' || p[1] == '"' : *p != ',')
          f ? (++ntb, C) : (++nb, H(C));
        p += 1 + q;
      }
      uint32_t rid = gi(p), cid = rid < 1000000 ? reg_lut[rid] : 0;

      // Exact fixed-point cents parsing without double-precision conversion
      neg = *p == '-' && ++p;
      while (*p != '.')
        cents = cents * 10 + (*p++ - '0');
      cents = (cents * 100 + p[1] * 10 + p[2] - 528) * (neg ? -1 : 1), p += 3;
      while (p < end && (uint8_t)*p <= ' ')
        ++p;

      T[tid][cid].add({1, 0, id, cents, cents, cents, nb, ntb});
      uint64_t h = nh ^ (nh >> 32) ^ (nb * 0x9e3779b97f4a7c15ULL);
      B[tid][cid % 12].push_back((uint64_t)cid << 48 | (h & 0xFFFFFFFFFFFFULL));
    }

#pragma omp barrier
    // Stage 4: Deduplicate distinct names per country shard with immediate
    // buffer free
    V s;
    for (auto &b : B)
      s.insert(s.end(), b[tid].begin(), b[tid].end()), V().swap(b[tid]);
    std::sort(s.begin(), s.end());
    for (uint64_t x : std::span(s.begin(), std::unique(s.begin(), s.end())))
      G[x >> 48].dn++;
  }

  // Cross-thread accumulation of local aggregates
  for (auto &t : T)
    for (int c = 0; c < 17577; ++c)
      if (t[c].cnt)
        G[c].add(t[c]);

  // Stage 5: Formatted buffered write to /out/aggregates.txt in ascending ASCII
  // order
  for (int cid = 0; cid < 17577; ++cid)
    if (Agg &g = G[cid < 17576 ? cid + 1 : 0]; g.cnt) {
      cid > 17575 ? (memcpy(o, "_UNMATCHED ", 11), o += 11)
                  : (o[0] = 'A' + cid / 676, o[1] = 'A' + cid / 26 % 26,
                     o[2] = 'A' + cid % 26, o[3] = ' ', o += 4);
      P(g.cnt), P(g.id_s), fc(g.am_s), fc(g.mi), fc(g.ma);
      P(g.dn), P(g.nb), P(g.ntb), o[-1] = '\n';
    }
  return write(open(c > 3 ? v[3] : "/out/aggregates.txt", 577, 438), buf,
               o - buf) < 0;
}
