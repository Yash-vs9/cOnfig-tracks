#include <bits/stdc++.h>
#include <fcntl.h>
#include <omp.h>
#include <sys/mman.h>
#include <unistd.h>

#define F(v, c) for (auto &v : c)
#define W(c) while (c)
#define C(s, n) (memcpy(ptr, s, n), ptr += n)
#define P(p) lats[g.off + (p * (uint64_t)g.stats[0] + 99) / 100 - 1]
#define M(g) (g.n != (r.n & 127) || g.d != r.d || memcmp(g.ep, ep, g.n))
#define O(i, f, m) open(c > i ? v[i] : f, m, 438)
#define H(p) __builtin_ia32_crc32di(*(size_t *)(p), *(size_t *)((p) + 5))
#define G(p) ((p[6] == '0' ? p[9] - '0' : p[9] - '0' + 1) * 24 + (p[11] - '0') * 10 + p[12] - '0' - 18)

#pragma pack(push, 1)
struct Rec {
  uint32_t p, l;
  uint16_t h;
  uint8_t o, n, d;
};
#pragma pack(pop)

struct Group {
  uint32_t off, stats[6];
  uint8_t n, d;
  const char *hr, *ep;
};

static std::vector<Rec> shard_buf[12][12];
static std::vector<Group> shard_out[12], hb[72];
static uint32_t s_lats[12][350000];
static char out_buf[12][2000000];
static size_t out_len[12];

int main(int c, char **v) {
  size_t fd = O(1, "/data/requests.log", 0), sz = lseek(fd, 0, 2);
  const char *base = sz ? (char *)mmap(0, sz, 1, 1, fd, 0) : 0;

#pragma omp parallel num_threads(12)
  {
    size_t t = omp_get_thread_num(), s = t * sz / 12, e = (t + 1) * sz / 12;
    for (int sh = 0; sh < 12; ++sh) shard_buf[t][sh].reserve(23500);
    W(s && s < sz && base[s - 1] != '\n')++ s;
    W(e && e < sz && base[e - 1] != '\n')++ e;

    for (const char *p = base + s, *end = base + e, *nl; p < end; p = nl + 1) {
      nl = (char *)memchr(p, '\n', end - p);
      const char *ep = (char *)memchr(p + 31, ' ', 8) + 1, *cur = nl - 1;
      uint32_t lat = 0, m = 1, h = H(p);
      W(*cur != ' ') lat += (*cur-- - '0') * m, m *= 10;
      uint8_t o = ep - p, err = cur[-3] == '5';
      uint8_t n = (cur - 4 - ep) | (err << 7), d = G(p);
      const char *lim = cur - 4;
      while (ep + 8 <= lim) h = (uint32_t)__builtin_ia32_crc32di(h, *(uint64_t *)ep), ep += 8;
      while (ep < lim) h = __builtin_ia32_crc32qi(h, *ep++);
      shard_buf[t][h % 12].push_back({uint32_t(p - base), lat, uint16_t(h), o, n, d});
    }

#pragma omp barrier
    uint32_t ht[65536], tot = (memset(ht, -1, sizeof(ht)), 0);
    auto &groups = shard_out[t];
    groups.reserve(16384);
    F(buf, shard_buf)
    F(r, buf[t]) {
      const char *hr = base + r.p, *ep = hr + r.o;
      uint32_t s = r.h & 65535, g;
      W(~(g = ht[s]) && M(groups[g])) s = (s + 1) & 65535;
      ~g ? (groups[g].stats[4] += (r.n >> 7), ++groups[g].stats[0], r.p = g)
         : (groups.push_back({0, {1, 0, 0, 0, uint32_t(r.n >> 7)}, uint8_t(r.n & 127), r.d, hr, ep}),
            r.p = ht[s] = groups.size() - 1);
    }

    F(g, groups) g.off = tot, tot += g.stats[0], g.stats[0] = 0;
    uint32_t *lats = s_lats[t];
    F(buf, shard_buf) {
      F(r, buf[t]) lats[groups[r.p].off + groups[r.p].stats[0]++] = r.l;
      std::vector<Rec>().swap(buf[t]);
    }
    F(g, groups) {
      std::sort(&lats[g.off], &lats[g.off] + g.stats[0]);
      g.stats[1] = P(50), g.stats[2] = P(95), g.stats[3] = P(99);
      g.stats[5] = (uint64_t)g.stats[4] * 1000000 / g.stats[0];
    }

#pragma omp barrier
#pragma omp single
    {
      F(b, hb) b.reserve(1850);
      F(s, shard_out) {
        F(g, s) hb[g.d].push_back(g);
        std::vector<Group>().swap(s);
      }
    }

    char *ptr = out_buf[t];
    for (int h = t * 6, end_h = h + 6; h < end_h; ++h) {
      auto &b = hb[h];
      std::sort(b.begin(), b.end(), [](auto &x, auto &y) {
        return std::string_view(x.ep, x.n) < std::string_view(y.ep, y.n);
      });
      F(s, b) {
        C(s.hr, 13), C(":00:00Z ", 8), C(s.ep, s.n), *ptr++ = ' ';
        F(val, s.stats) ptr = std::to_chars(ptr, ptr + 12, val).ptr, *ptr++ = ' ';
        ptr[-1] = '\n';
      }
      std::vector<Group>().swap(b);
    }
    out_len[t] = ptr - out_buf[t];
  }

  int out_fd = O(2, "/out/summary.txt", 577);
  for (int t = 0; t < 12; ++t) write(out_fd, out_buf[t], out_len[t]);
  return 0;
}
