#include <bits/stdc++.h>
#include <fcntl.h>
#include <nmmintrin.h>
#include <omp.h>
#include <sys/mman.h>
#include <sys/stat.h>

struct __attribute__((packed)) Rec { uint32_t lat; uint16_t eid; };

static uint64_t hr_key[256], ep_key[8192];
static const char *hr_ptr[144], *ep_ptr[2048];
static uint8_t hr_id[256], ep_len[2048], ord_h[144];
static uint16_t ep_id[8192], ord_e[2048], blk_fill[20480];
static std::atomic<uint32_t> n_hr{0}, n_ep{0}, n_blks{0};

constexpr uint32_t BLK_SZ = 256;
constexpr uint32_t NUM_BLKS = 20480;
static Rec pool[NUM_BLKS * BLK_SZ];
static int32_t blk_next[NUM_BLKS], head_blk[12][144], cur_blk[12][144];
static uint32_t g_err[144 * 2048], t_out_len[12];
static char t_out[12][360000];

static inline uint8_t get_hid(const char *p) {
  uint64_t k1, k2;
  memcpy(&k1, p, 8), memcpy(&k2, p + 5, 8);
  uint64_t h = _mm_crc32_u64(k1, k2);
  if (h <= 1) h = 2;
  for (uint32_t s = h & 255;; s = (s + 1) & 255) {
    uint64_t k = __atomic_load_n(&hr_key[s], __ATOMIC_ACQUIRE);
    if (k == h && memcmp(hr_ptr[hr_id[s]], p, 13) == 0) return hr_id[s];
    if (!k) {
      uint64_t exp = 0;
      if (__atomic_compare_exchange_n(&hr_key[s], &exp, 1ULL, false, __ATOMIC_ACQ_REL, __ATOMIC_ACQUIRE)) {
        uint8_t id = (uint8_t)n_hr.fetch_add(1, std::memory_order_relaxed);
        hr_ptr[id] = p, hr_id[s] = id;
        __atomic_store_n(&hr_key[s], h, __ATOMIC_RELEASE);
        return id;
      }
    }
    while ((k = __atomic_load_n(&hr_key[s], __ATOMIC_ACQUIRE)) == 1ULL) _mm_pause();
    if (k == h && memcmp(hr_ptr[hr_id[s]], p, 13) == 0) return hr_id[s];
  }
}

static inline uint16_t get_eid(const char *ep, uint8_t len) {
  uint64_t h = len + 1, w = 0;
  uint8_t k = 0;
  for (; k + 8 <= len; k += 8) memcpy(&w, ep + k, 8), h = _mm_crc32_u64(h, w);
  while (k < len) h = _mm_crc32_u8(h, (uint8_t)ep[k++]);
  h = (h << 8) | len | (1ULL << 62);
  for (uint32_t s = (uint32_t)(h >> 8) & 8191;; s = (s + 1) & 8191) {
    uint64_t key = __atomic_load_n(&ep_key[s], __ATOMIC_ACQUIRE);
    if (key == h && ep_len[ep_id[s]] == len && memcmp(ep_ptr[ep_id[s]], ep, len) == 0) return ep_id[s];
    if (!key) {
      uint64_t exp = 0;
      if (__atomic_compare_exchange_n(&ep_key[s], &exp, 1ULL, false, __ATOMIC_ACQ_REL, __ATOMIC_ACQUIRE)) {
        uint16_t id = (uint16_t)n_ep.fetch_add(1, std::memory_order_relaxed);
        ep_ptr[id] = ep, ep_len[id] = len, ep_id[s] = id;
        __atomic_store_n(&ep_key[s], h, __ATOMIC_RELEASE);
        return id;
      }
    }
    while ((key = __atomic_load_n(&ep_key[s], __ATOMIC_ACQUIRE)) == 1ULL) _mm_pause();
    if (key == h && ep_len[ep_id[s]] == len && memcmp(ep_ptr[ep_id[s]], ep, len) == 0) return ep_id[s];
  }
}

int main(int argc, char **argv) {
  int fd = open(argc > 1 ? argv[1] : "/data/requests.log", O_RDONLY);
  int out_fd = open(argc > 2 ? argv[2] : "/out/summary.txt", O_WRONLY | O_CREAT | O_TRUNC, 0666);
  struct stat st{};
  fstat(fd, &st);
  size_t sz = st.st_size;
  const char *base = sz ? (const char *)mmap(0, sz, PROT_READ, MAP_SHARED, fd, 0) : nullptr;
  if (sz && base == MAP_FAILED) base = (const char *)mmap(0, sz, PROT_READ, MAP_PRIVATE, fd, 0);
  memset(head_blk, -1, sizeof(head_blk)), memset(cur_blk, -1, sizeof(cur_blk));
  uint32_t nh = 0, ne = 0;

#pragma omp parallel num_threads(12)
  {
    int tid = omp_get_thread_num();
    size_t s = tid ? (tid * sz) / 12 : 0, e = tid < 11 ? ((tid + 1) * sz) / 12 : sz;
    while (s && base[s - 1] != '\n') ++s;
    while (e < sz && base[e - 1] != '\n') ++e;
    const char *last_hr = nullptr;
    uint8_t hid = 0;

    for (const char *p = base + s, *end_c = base + e; p < end_c;) {
      const char *nl = (const char *)memchr(p, '\n', end_c - p), *ep = p + 31, *cur = nl - 1;
      while (*ep++ != ' ') {}
      uint32_t lat = 0, mult = 1;
      while (*cur != ' ') lat += (*cur-- - '0') * mult, mult *= 10;
      uint8_t elen = (cur - 4) - ep;
      if (!last_hr || memcmp(p, last_hr, 13) != 0) last_hr = p, hid = get_hid(p);
      uint16_t eid = get_eid(ep, elen);
      if (cur[-3] == '5') __atomic_fetch_add(&g_err[(uint32_t)hid * 2048 + eid], 1U, __ATOMIC_RELAXED);
      int32_t b = cur_blk[tid][hid];
      if (b < 0 || blk_fill[b] == BLK_SZ) {
        int32_t nb = (int32_t)n_blks.fetch_add(1, std::memory_order_relaxed);
        blk_next[nb] = -1, blk_fill[nb] = 0;
        (b < 0 ? head_blk[tid][hid] : blk_next[b]) = nb;
        cur_blk[tid][hid] = b = nb;
      }
      pool[b * BLK_SZ + blk_fill[b]++] = {lat, eid};
      p = nl + 1;
    }
#pragma omp barrier
#pragma omp single
    {
      nh = n_hr.load(), ne = n_ep.load();
      std::iota(ord_h, ord_h + nh, 0), std::iota(ord_e, ord_e + ne, 0);
      std::sort(ord_h, ord_h + nh, [](uint8_t a, uint8_t b) { return memcmp(hr_ptr[a], hr_ptr[b], 13) < 0; });
      std::sort(ord_e, ord_e + ne, [](uint16_t a, uint16_t b) {
        return std::string_view(ep_ptr[a], ep_len[a]) < std::string_view(ep_ptr[b], ep_len[b]);
      });
    }

    uint32_t cnt[2048], off[2048];
    std::vector<uint32_t> lats;
    char *out = t_out[tid];
    for (uint32_t base_hi = 0; base_hi < nh; base_hi += 12) {
      uint32_t hi = base_hi + tid, out_len = 0;
      if (hi < nh) {
        uint8_t h_id = ord_h[hi];
        memset(cnt, 0, ne * sizeof(uint32_t));
        uint32_t tot = 0;
        for (int t = 0; t < 12; ++t)
          for (int32_t b = head_blk[t][h_id]; b >= 0; b = blk_next[b])
            for (uint16_t i = 0; i < blk_fill[b]; ++i) cnt[pool[b * BLK_SZ + i].eid]++;
        for (uint32_t i = 0; i < ne; ++i) off[i] = tot, tot += cnt[i], cnt[i] = 0;
        lats.resize(tot);
        for (int t = 0; t < 12; ++t)
          for (int32_t b = head_blk[t][h_id]; b >= 0; b = blk_next[b])
            for (uint16_t i = 0; i < blk_fill[b]; ++i) {
              const Rec &r = pool[b * BLK_SZ + i];
              lats[off[r.eid] + cnt[r.eid]++] = r.lat;
            }
        char *ptr = out;
        for (uint32_t ei = 0; ei < ne; ++ei) {
          uint16_t eid = ord_e[ei];
          uint32_t n = cnt[eid];
          if (!n) continue;
          uint32_t *L = lats.data() + off[eid];
          uint32_t i50 = (50ULL * n + 99) / 100 - 1, i95 = (95ULL * n + 99) / 100 - 1, i99 = (99ULL * n + 99) / 100 - 1;
          if (n <= 32) std::sort(L, L + n);
          else {
            std::nth_element(L, L + i50, L + n);
            if (i95 > i50) std::nth_element(L + i50 + 1, L + i95, L + n);
            if (i99 > i95) std::nth_element(L + i95 + 1, L + i99, L + n);
          }
          uint32_t err = g_err[(uint32_t)h_id * 2048 + eid], ppm = (uint32_t)((uint64_t)err * 1000000ULL / n);
          memcpy(ptr, hr_ptr[h_id], 13), ptr += 13;
          memcpy(ptr, ":00:00Z ", 8), ptr += 8;
          memcpy(ptr, ep_ptr[eid], ep_len[eid]), ptr += ep_len[eid], *ptr++ = ' ';
          for (uint32_t v : {n, L[i50], L[i95], L[i99], err, ppm})
            ptr = std::to_chars(ptr, ptr + 12, v).ptr, *ptr++ = ' ';
          ptr[-1] = '\n';
        }
        out_len = ptr - out;
      }
      t_out_len[tid] = out_len;
#pragma omp barrier
#pragma omp single
      for (uint32_t k = 0; k < 12 && base_hi + k < nh; ++k)
        if (t_out_len[k]) write(out_fd, t_out[k], t_out_len[k]);
    }
  }
  close(out_fd);
  return 0;
}
