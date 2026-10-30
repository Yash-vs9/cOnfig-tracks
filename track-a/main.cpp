#include <bits/stdc++.h>
#include <fcntl.h>
#include <omp.h>
#include <sys/mman.h>

#define F(v, c) for (auto &v : c)
#define W(c) while (c)
#define C(s, n) (memcpy(ptr, s, n), ptr += n)
#define P(p) lats[g.off + (p * (uint64_t)g.stats[0] + 99) / 100 - 1]
#define V(x) std::pair(std::string_view(x.hr, 13), std::string_view(x.ep, x.n))
#define O(i, f, m) open(c > i ? v[i] : f, m, 438)
#define M(g) (g.n != r.n || memcmp(g.hr, hr, 13) || memcmp(g.ep, ep, r.n))
// Overlapping 8-byte hardware CRC32C hour hash covering 13-byte ISO timestamp
// prefix
#define H(p) __builtin_ia32_crc32di(*(size_t *)(p), *(size_t *)((p) + 5))

struct Rec {
  uint32_t h, p, l;
  uint8_t o, n, e;
};
struct Group {
  uint32_t off, stats[6], n;
  const char *hr, *ep;
};

static std::vector<Rec> shard_buf[12][12];
static std::vector<Group> shard_out[12];

int main(int c, char **v) {
  // Stage 1: Zero-copy mmap of requests.log over pre-warmed OS buffer cache
  size_t fd = O(1, "/data/requests.log", 0), sz = lseek(fd, 0, 2);
  const char *base = sz ? (char *)mmap(0, sz, 1, 1, fd, 0) : 0;

#pragma omp parallel num_threads(12)
  {
    size_t t = omp_get_thread_num(), s = t * sz / 12, e = (t + 1) * sz / 12;
    W(s && s < sz && base[s - 1] != '\n')++ s;
    W(e && e < sz && base[e - 1] != '\n')++ e;

    // Stage 2: Backward latency/status parsing and hardware CRC32C endpoint
    // hashing
    for (const char *p = base + s, *end = base + e, *nl; p < end; p = nl + 1) {
      nl = (char *)memchr(p, '\n', end - p);
      const char *ep = (char *)memchr(p + 31, ' ', 8) + 1, *cur = nl - 1;
      uint32_t lat = 0, m = 1, h = H(p);
      W(*cur != ' ') lat += (*cur-- - '0') * m, m *= 10;
      uint8_t o = ep - p, n = cur - 4 - ep, err = cur[-3] == '5';
      W(ep < cur - 4) h = __builtin_ia32_crc32qi(h, *ep++);
      shard_buf[t][h % 12].push_back({h, uint32_t(p - base), lat, o, n, err});
    }

#pragma omp barrier
    // Stage 3: Two-stage lock-free flat CSR latency aggregation and percentile
    // calculation
    uint32_t ht[65536], tot = (memset(ht, -1, sizeof(ht)), 0);
    auto &groups = shard_out[t];
    F(buf, shard_buf)
    F(r, buf[t]) {
      const char *hr = base + r.p, *ep = hr + r.o;
      uint32_t s = r.h & 65535, g;
      W(~(g = ht[s]) && M(groups[g])) s = (s + 1) & 65535;
      ~g ? (groups[g].stats[4] += r.e, ++groups[g].stats[0], r.h = g)
         : (groups.push_back({0, {1, 0, 0, 0, r.e}, r.n, hr, ep}),
            r.h = ht[s] = groups.size() - 1);
    }

    F(g, groups) g.off = tot, tot += g.stats[0], g.stats[0] = 0;
    std::vector<uint32_t> lats(tot);
    F(buf, shard_buf) {
      F(r, buf[t]) lats[groups[r.h].off + groups[r.h].stats[0]++] = r.l;
      std::vector<Rec>().swap(buf[t]);
    }
    F(g, groups) {
      std::sort(&lats[g.off], &lats[g.off] + g.stats[0]);
      g.stats[1] = P(50), g.stats[2] = P(95), g.stats[3] = P(99);
      g.stats[5] = (uint64_t)g.stats[4] * 1000000 / g.stats[0];
    }
  }

  // Stage 4: Ordered output buffer formatting and atomic direct write
  auto &A = shard_out[0];
  F(u, std::span(shard_out + 1, 11)) A.insert(A.end(), u.begin(), u.end());
  std::sort(A.begin(), A.end(), [](auto &a, auto &b) { return V(a) < V(b); });
  char *buf = (char *)malloc(A.size() * 256), *ptr = buf;
  F(s, A) {
    C(s.hr, 13), C(":00:00Z ", 8), C(s.ep, s.n), *ptr++ = ' ';
    F(val, s.stats) ptr = std::to_chars(ptr, ptr + 12, val).ptr, *ptr++ = ' ';
    ptr[-1] = '\n';
  }
  return write(O(2, "/out/summary.txt", 577), buf, ptr - buf) < 0;
}
