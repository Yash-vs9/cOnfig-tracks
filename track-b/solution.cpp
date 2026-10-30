#pragma GCC optimize("O3,unroll-loops")
#pragma GCC target("avx2,bmi,bmi2,lzcnt,popcnt")
#include <fcntl.h>
#include <immintrin.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>
#include <algorithm>
#include <atomic>
#include <charconv>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <thread>
#include <vector>

static constexpr uint32_t H_SZ = 131072, H_MSK = H_SZ - 1, BS_TH = 15625, NUM_C = 12, MAX_W = 384;
alignas(64) static uint8_t cmap[256];
struct Slot { uint32_t h32, tid; } static vtab[H_SZ];
struct TInfo { uint64_t w0, w1; uint32_t off; uint16_t len; } static tinfo[81920];
alignas(64) static uint64_t bloom[8192];
static uint32_t n_terms = 0, total_docs = 0, doc_s[NUM_C], doc_e[NUM_C], c_base[NUM_C], n_thr = 1, w_per_doc = 0, n_bs = 0;
static size_t c_byte[NUM_C + 1];
struct Query { uint8_t kind, k; uint16_t ops; uint32_t tids[8]; };
static std::vector<Query> queries;
static std::vector<uint32_t> rep_map, term_df, bs_idx, c_off[NUM_C];
static std::vector<uint8_t> is_bs;
static uint32_t* doc_ts = nullptr;
static uint64_t* g_bs = nullptr;
static uint16_t* g_u16 = nullptr;
struct Slice { uint32_t off, len; };
static std::vector<Slice> q_res;
static char* res_buf = nullptr;
static std::atomic<uint32_t> res_ptr{0};

static inline uint64_t mix64(uint64_t w0, uint64_t w1, uint32_t len) {
    uint64_t h = (w0 ^ ((w1 * 0x9e3779b97f4a7c15ULL) + len)) * 0xbf58476d1ce4e5b9ULL;
    return h ^ (h >> 31);
}

static inline uint32_t lookup_swar(uint64_t w0, uint64_t w1, const char* tail, uint32_t len, const char* q_buf, uint64_t h) {
    uint32_t h32 = (uint32_t)(h ^ (h >> 32)) | 1u, idx = (uint32_t)h & H_MSK;
    while (vtab[idx].h32) {
        if (vtab[idx].h32 == h32) {
            const auto& ti = tinfo[vtab[idx].tid];
            if (ti.w0 == w0 && ti.w1 == w1 && ti.len == len && (__builtin_expect(len <= 16, 1) || !memcmp(q_buf + ti.off + 16, tail, len - 16)))
                return vtab[idx].tid;
        }
        idx = (idx + 1) & H_MSK;
    }
    return UINT32_MAX;
}

static inline uint32_t insert_term(const char* s, uint32_t len, const char* q_buf) {
    uint64_t w0 = 0, w1 = 0;
    memcpy(&w0, s, std::min(len, 8u));
    if (len > 8) memcpy(&w1, s + 8, std::min(len - 8u, 8u));
    uint64_t h = mix64(w0, w1, len);
    uint32_t tid = lookup_swar(w0, w1, s + 16, len, q_buf, h);
    if (tid != UINT32_MAX) return tid;
    uint32_t h32 = (uint32_t)(h ^ (h >> 32)) | 1u, idx = (uint32_t)h & H_MSK;
    while (vtab[idx].h32) idx = (idx + 1) & H_MSK;
    tid = n_terms++;
    vtab[idx] = {h32, tid};
    tinfo[tid] = {w0, w1, (uint32_t)(s - q_buf), (uint16_t)len};
    bloom[(h >> 32) & 8191] |= 1ULL << (h & 63);
    return tid;
}

template <typename F>
static void run_par(uint32_t n, uint32_t b, F&& fn) {
    std::atomic<uint32_t> idx{0};
    std::vector<std::thread> th;
    for (uint32_t t = 0; t < n_thr; ++t) th.emplace_back([&]() {
        while (true) {
            uint32_t i = idx.fetch_add(b, std::memory_order_relaxed);
            if (i >= n) break;
            for (uint32_t j = i, e = std::min(n, i + b); j < e; ++j) fn(j);
        }
    });
    for (auto& w : th) w.join();
}

static inline void read_tok(const char*& p, uint64_t& w0, uint64_t& w1, char* tail, uint32_t& len) {
    w0 = w1 = len = 0;
    uint8_t m = cmap[(uint8_t)*p];
    do {
        if (len < 8) w0 |= (uint64_t)m << (len * 8);
        else if (len < 16) w1 |= (uint64_t)m << ((len - 8) * 8);
        else if (len < 32) tail[len - 16] = (char)m;
        len++; m = cmap[(uint8_t)*++p];
    } while (m > 1);
}

template <typename F>
static inline void scan_doc(const char*& p, const char* q_buf, F&& fn) {
    uint32_t pos = 0, p_tid = UINT32_MAX, p_pos = UINT32_MAX;
    while (true) {
        while (!cmap[(uint8_t)*p]) p++;
        if (*p == '\n') { p++; break; }
        uint64_t w0, w1; char tail[16]; uint32_t len;
        read_tok(p, w0, w1, tail, len);
        uint64_t h = mix64(w0, w1, len);
        uint32_t tid = ((bloom[(h >> 32) & 8191] >> (h & 63)) & 1ULL) ? lookup_swar(w0, w1, tail, len, q_buf, h) : UINT32_MAX;
        if (tid != UINT32_MAX) { fn(tid, p_tid == tid && pos == p_pos + 1); p_tid = tid; p_pos = pos; }
        else p_tid = UINT32_MAX;
        pos++;
    }
}

static void pass1(uint32_t c, const char* d_buf, const char* q_buf, uint32_t* cnts, uint32_t* seen) {
    if (doc_s[c] >= doc_e[c]) return;
    memset(seen, 0xFF, n_terms * 4);
    const char *p = d_buf + c_byte[c], *pe = d_buf + c_byte[c + 1];
    uint32_t r_sz = (uint32_t)rep_map.size();
    const uint32_t* r_map = rep_map.data();
    while (p < pe) {
        uint32_t d = 0; while (*p >= '0' && *p <= '9') d = d * 10 + (*p++ - '0');
        if (*p == '\t') p++;
        doc_ts[d] = (uint32_t)(p - d_buf);
        scan_doc(p, q_buf, [&](uint32_t tid, bool rep) {
            if (seen[tid] != d) { cnts[tid]++; seen[tid] = d; }
            if (__builtin_expect(rep, 0) && tid < r_sz && r_map[tid] != UINT32_MAX && seen[r_map[tid]] != d) {
                cnts[r_map[tid]]++; seen[r_map[tid]] = d;
            }
        });
    }
}

// Pass 2 requires ZERO `last_seen` memory: bitsets are idempotent and u16 postings are monotonic!
static void pass2(uint32_t c, const char* d_buf, const char* q_buf) {
    uint32_t d0 = doc_s[c]; if (d0 >= doc_e[c]) return;
    const char *p = d_buf + c_byte[c], *pe = d_buf + c_byte[c + 1];
    uint32_t* co = c_off[c].data();
    std::vector<uint32_t> start_off = c_off[c];
    uint32_t r_sz = (uint32_t)rep_map.size();
    const uint32_t* r_map = rep_map.data();
    auto emit = [&](uint32_t tid, uint32_t d, uint16_t ld) {
        if (is_bs[tid]) g_bs[(size_t)bs_idx[tid] * w_per_doc + (d >> 6)] |= 1ULL << (d & 63);
        else if (co[tid] == start_off[tid] || g_u16[co[tid] - 1] != ld) g_u16[co[tid]++] = ld;
    };
    while (p < pe) {
        uint32_t d = 0; while (*p >= '0' && *p <= '9') d = d * 10 + (*p++ - '0');
        if (*p == '\t') p++;
        uint16_t ld = (uint16_t)(d - d0);
        scan_doc(p, q_buf, [&](uint32_t tid, bool rep) {
            emit(tid, d, ld);
            if (__builtin_expect(rep, 0) && tid < r_sz && r_map[tid] != UINT32_MAX) emit(r_map[tid], d, ld);
        });
    }
}

static inline void get_c_bs(uint32_t tid, uint32_t c, uint32_t cw, uint64_t* out) {
    if (tid == UINT32_MAX) { memset(out, 0, cw * 8); return; }
    if (is_bs[tid]) memcpy(out, &g_bs[(size_t)bs_idx[tid] * w_per_doc + (doc_s[c] >> 6)], cw * 8);
    else {
        memset(out, 0, cw * 8);
        uint32_t s = tid ? c_off[c][tid - 1] : c_base[c], e = c_off[c][tid];
        for (uint32_t i = s; i < e; ++i) out[g_u16[i] >> 6] |= 1ULL << (g_u16[i] & 63);
    }
}

static inline void fmt_res(uint64_t cnt, uint64_t sum, const uint32_t* top, uint32_t tc, Slice& sl) {
    char buf[512], *p = buf;
    if (!cnt) { memcpy(p, "0 0\n", 4); p += 4; }
    else {
        p = std::to_chars(p, buf + 400, cnt).ptr; *p++ = ' ';
        p = std::to_chars(p, buf + 400, sum).ptr;
        for (uint32_t i = 0; i < tc; ++i) { *p++ = ' '; p = std::to_chars(p, buf + 400, top[i]).ptr; }
        *p++ = '\n';
    }
    uint32_t len = (uint32_t)(p - buf), off = res_ptr.fetch_add(len, std::memory_order_relaxed);
    memcpy(res_buf + off, buf, len); sl = {off, len};
}

static void exec_bool(const Query& q, Slice& sl) {
    uint64_t t_cnt = 0, t_sum = 0; uint32_t top[10], tc = 0;
    for (uint32_t c = 0; c < NUM_C; ++c) {
        uint32_t cs = doc_s[c], cd = doc_e[c] - cs; if (!cd) continue;
        uint32_t cw = (cd + 63) >> 6, t0 = q.tids[0], clen = 0;
        uint16_t clist[1024]; bool in_bs = is_bs[t0];
        alignas(32) uint64_t curr[MAX_W], nb[MAX_W];
        if (in_bs) get_c_bs(t0, c, cw, curr);
        else {
            uint32_t s0 = t0 ? c_off[c][t0 - 1] : c_base[c]; clen = c_off[c][t0] - s0;
            if (clen > 1024) { in_bs = true; get_c_bs(t0, c, cw, curr); }
            else memcpy(clist, g_u16 + s0, clen * 2);
        }
        for (uint8_t oi = 0; oi < q.k - 1; ++oi) {
            uint8_t op = (q.ops >> (oi * 2)) & 3;
            uint32_t nt = q.tids[oi + 1], ns = nt ? c_off[c][nt - 1] : c_base[c], ne = c_off[c][nt];
            if (in_bs) {
                if (is_bs[nt]) {
                    const uint64_t* b = &g_bs[(size_t)bs_idx[nt] * w_per_doc + (cs >> 6)];
                    if (op == 1) for (uint32_t w = 0; w < cw; ++w) curr[w] &= b[w];
                    else if (op == 2) for (uint32_t w = 0; w < cw; ++w) curr[w] |= b[w];
                    else for (uint32_t w = 0; w < cw; ++w) curr[w] &= ~b[w];
                } else if (op == 2) for (uint32_t i = ns; i < ne; ++i) curr[g_u16[i] >> 6] |= 1ULL << (g_u16[i] & 63);
                else if (op == 3) for (uint32_t i = ns; i < ne; ++i) curr[g_u16[i] >> 6] &= ~(1ULL << (g_u16[i] & 63));
                else { get_c_bs(nt, c, cw, nb); for (uint32_t w = 0; w < cw; ++w) curr[w] &= nb[w]; }
            } else if (op == 1) {
                if (!clen) continue;
                uint32_t w = 0;
                if (is_bs[nt]) {
                    const uint64_t* b = &g_bs[(size_t)bs_idx[nt] * w_per_doc];
                    for (uint32_t i = 0; i < clen; ++i) { uint32_t d = cs + clist[i]; if ((b[d >> 6] >> (d & 63)) & 1ULL) clist[w++] = clist[i]; }
                } else for (uint32_t i = 0, j = ns; i < clen && j < ne;)
                    if (clist[i] == g_u16[j]) { clist[w++] = clist[i++]; j++; } else if (clist[i] < g_u16[j]) i++; else j++;
                clen = w;
            } else if (op == 3) {
                if (!clen) continue;
                uint32_t w = 0;
                if (is_bs[nt]) {
                    const uint64_t* b = &g_bs[(size_t)bs_idx[nt] * w_per_doc];
                    for (uint32_t i = 0; i < clen; ++i) { uint32_t d = cs + clist[i]; if (!((b[d >> 6] >> (d & 63)) & 1ULL)) clist[w++] = clist[i]; }
                } else {
                    uint32_t i = 0, j = ns;
                    while (i < clen && j < ne) if (clist[i] == g_u16[j]) { i++; j++; } else if (clist[i] < g_u16[j]) clist[w++] = clist[i++]; else j++;
                    while (i < clen) clist[w++] = clist[i++];
                }
                clen = w;
            } else {
                uint32_t nlen = is_bs[nt] ? 100000 : (ne - ns);
                if (is_bs[nt] || clen + nlen > 1024) {
                    get_c_bs(nt, c, cw, curr);
                    for (uint32_t i = 0; i < clen; ++i) curr[clist[i] >> 6] |= 1ULL << (clist[i] & 63);
                    in_bs = true;
                } else {
                    uint16_t tmp[1024]; uint32_t i = 0, j = ns, w = 0;
                    while (i < clen && j < ne) if (clist[i] == g_u16[j]) { tmp[w++] = clist[i++]; j++; } else if (clist[i] < g_u16[j]) tmp[w++] = clist[i++]; else tmp[w++] = g_u16[j++];
                    while (i < clen) tmp[w++] = clist[i++];
                    while (j < ne) tmp[w++] = g_u16[j++];
                    memcpy(clist, tmp, w * 2); clen = w;
                }
            }
        }
        if (!in_bs) {
            t_cnt += clen;
            for (uint32_t i = 0; i < clen; ++i) { uint32_t d = cs + clist[i]; t_sum += d; if (__builtin_expect(tc < 10, 0)) top[tc++] = d; }
        } else {
            if (cd & 63) curr[cw - 1] &= (1ULL << (cd & 63)) - 1ULL;
            for (uint32_t w = 0; w < cw; ++w) {
                uint64_t v = curr[w]; if (!v) continue;
                t_cnt += __builtin_popcountll(v);
                for (uint32_t b = cs + (w << 6); v; v &= v - 1) {
                    uint32_t d = b + __builtin_ctzll(v); t_sum += d; if (__builtin_expect(tc < 10, 0)) top[tc++] = d;
                }
            }
        }
    }
    fmt_res(t_cnt, t_sum, top, tc, sl);
}

static void exec_phrase(const Query& q, const char* d_buf, const char* q_buf, Slice& sl) {
    uint8_t k = q.k, nd = 0; uint32_t dist[16];
    for (uint8_t i = 0; i < k; ++i) {
        uint32_t t = q.tids[i]; if (t == UINT32_MAX || !term_df[t]) { fmt_res(0, 0, nullptr, 0, sl); return; }
        if (std::find(dist, dist + nd, t) == dist + nd) dist[nd++] = t;
        if (i + 1 < k && t == q.tids[i + 1] && t < rep_map.size() && rep_map[t] != UINT32_MAX) {
            uint32_t rt = rep_map[t]; if (!term_df[rt]) { fmt_res(0, 0, nullptr, 0, sl); return; }
            if (std::find(dist, dist + nd, rt) == dist + nd) dist[nd++] = rt;
        }
    }
    std::sort(dist, dist + nd, [](uint32_t a, uint32_t b) { return term_df[a] < term_df[b]; });
    TInfo pt[8]; for (uint8_t i = 0; i < k; ++i) pt[i] = tinfo[q.tids[i]];
    uint64_t t_cnt = 0, t_sum = 0; uint32_t top[10], tc = 0; uint16_t cand[21000];

    for (uint32_t c = 0; c < NUM_C; ++c) {
        uint32_t cs = doc_s[c], cd = doc_e[c] - cs; if (!cd) continue;
        uint32_t rare = dist[0], clen = 0; const uint16_t* cptr = cand;
        if (!is_bs[rare]) {
            uint32_t s = rare ? c_off[c][rare - 1] : c_base[c]; clen = c_off[c][rare] - s;
            if (!clen) continue;
            if (nd == 1) cptr = g_u16 + s; else memcpy(cand, g_u16 + s, clen * 2);
        } else {
            const uint64_t* src = &g_bs[(size_t)bs_idx[rare] * w_per_doc + (cs >> 6)];
            for (uint32_t w = 0, cw = (cd + 63) >> 6; w < cw; ++w)
                for (uint64_t v = src[w]; v; v &= v - 1) { uint32_t id = (w << 6) + __builtin_ctzll(v); if (id < cd) cand[clen++] = (uint16_t)id; }
            if (!clen) continue;
        }
        for (uint8_t ti = 1; ti < nd && clen; ++ti) {
            uint32_t ot = dist[ti], w = 0;
            if (is_bs[ot]) {
                const uint64_t* b = &g_bs[(size_t)bs_idx[ot] * w_per_doc];
                for (uint32_t i = 0; i < clen; ++i) { uint32_t d = cs + cand[i]; if ((b[d >> 6] >> (d & 63)) & 1ULL) cand[w++] = cand[i]; }
            } else {
                uint32_t ns = ot ? c_off[c][ot - 1] : c_base[c], ne = c_off[c][ot];
                for (uint32_t i = 0, j = ns; i < clen && j < ne;)
                    if (cand[i] == g_u16[j]) { cand[w++] = cand[i++]; j++; } else if (cand[i] < g_u16[j]) i++; else j++;
            }
            clen = w;
        }
        // 1-Cycle SWAR 64-bit integer comparison instead of byte-by-byte loop!
        for (uint32_t ci = 0; ci < clen; ++ci) {
            uint32_t d = cs + cptr[ci]; const char* p = d_buf + doc_ts[d]; bool m = false;
            while (true) {
                while (!cmap[(uint8_t)*p]) p++;
                if (*p == '\n') break;
                uint64_t w0, w1; char tail[16]; uint32_t l;
                read_tok(p, w0, w1, tail, l);
                if (l == pt[0].len && w0 == pt[0].w0 && w1 == pt[0].w1 && (__builtin_expect(l <= 16, 1) || !memcmp(q_buf + pt[0].off + 16, tail, l - 16))) {
                    const char* sp = p; uint8_t j = 1;
                    for (; j < k; ++j) {
                        while (!cmap[(uint8_t)*sp]) sp++;
                        if (*sp == '\n') break;
                        uint64_t jw0, jw1; char jtail[16]; uint32_t jl;
                        read_tok(sp, jw0, jw1, jtail, jl);
                        if (jl != pt[j].len || jw0 != pt[j].w0 || jw1 != pt[j].w1 || (__builtin_expect(jl > 16, 0) && memcmp(q_buf + pt[j].off + 16, jtail, jl - 16))) break;
                    }
                    if (j == k) { m = true; break; }
                }
            }
            if (m) { t_cnt++; t_sum += d; if (__builtin_expect(tc < 10, 0)) top[tc++] = d; }
        }
    }
    fmt_res(t_cnt, t_sum, top, tc, sl);
}

static const char* map_f(const char* path, size_t& sz) {
    int fd = open(path, O_RDONLY); if (fd < 0) return nullptr;
    struct stat st; fstat(fd, &st); sz = st.st_size;
    const char* p = (const char*)mmap(nullptr, sz, PROT_READ, MAP_SHARED, fd, 0);
    close(fd); return p;
}

int main(int argc, char** argv) {
    cmap['\n'] = 1;
    for (int i = '0'; i <= '9'; ++i) cmap[i] = i;
    for (int i = 'a'; i <= 'z'; ++i) cmap[i] = i, cmap[i - 32] = i;
    const char *d_dir = "/data", *o_dir = "/out";
    for (int i = 1; i + 1 < argc; i += 2) if (!strcmp(argv[i], "--data")) d_dir = argv[i + 1]; else if (!strcmp(argv[i], "--out")) o_dir = argv[i + 1];
    char dp[512], qp[512], rp[512];
    snprintf(dp, 512, "%s/docs.txt", d_dir); snprintf(qp, 512, "%s/queries.txt", d_dir); snprintf(rp, 512, "%s/results.txt", o_dir);
    size_t q_sz = 0, f_sz = 0;
    const char *q_buf = map_f(qp, q_sz), *d_buf = map_f(dp, f_sz);
    if (!q_buf || !d_buf) return 1;

    for (const char *qp_ = q_buf, *qe = q_buf + q_sz; qp_ < qe;) {
        while (qp_ < qe && (*qp_ == ' ' || *qp_ == '\r' || *qp_ == '\n')) qp_++;
        if (qp_ >= qe) break;
        bool ib = !strncmp(qp_, "BOOL ", 5); qp_ += ib ? 5 : 8;
        const char* le = (const char*)memchr(qp_, ib ? '\n' : '\"', qe - qp_); if (!le) le = qe;
        Query q = {(uint8_t)!ib, 0, 0, {0}};
        while (qp_ < le) {
            while (qp_ < le && *qp_ == ' ') qp_++;
            if (qp_ >= le) break;
            const char* tk = qp_; while (qp_ < le && *qp_ != ' ') qp_++;
            uint32_t l = (uint32_t)(qp_ - tk);
            if (ib && l == 3 && !memcmp(tk, "AND", 3)) q.ops |= 1 << ((q.k - 1) * 2);
            else if (ib && l == 2 && !memcmp(tk, "OR", 2)) q.ops |= 2 << ((q.k - 1) * 2);
            else if (ib && l == 3 && !memcmp(tk, "NOT", 3)) q.ops |= 3 << ((q.k - 1) * 2);
            else q.tids[q.k++] = insert_term(tk, l, q_buf);
        }
        queries.push_back(q);
        qp_ = (const char*)memchr(le, '\n', qe - le); if (qp_) qp_++; else break;
    }
    rep_map.assign(n_terms, UINT32_MAX);
    for (const auto& q : queries) if (q.kind) for (uint8_t i = 0; i + 1 < q.k; ++i)
        if (q.tids[i] == q.tids[i + 1] && rep_map[q.tids[i]] == UINT32_MAX) {
            uint32_t rt = n_terms++; rep_map[q.tids[i]] = rt; tinfo[rt] = tinfo[q.tids[i]];
        }

    const char* pl = d_buf + f_sz - 1; while (pl > d_buf && *(pl - 1) != '\n') pl--;
    while (pl < d_buf + f_sz && *pl >= '0' && *pl <= '9') total_docs = total_docs * 10 + (*pl++ - '0');
    total_docs++;
    doc_ts = (uint32_t*)malloc(total_docs * 4);
    uint32_t dpc = (total_docs + NUM_C - 1) / NUM_C;
    c_byte[0] = 0; c_byte[NUM_C] = f_sz;
    for (uint32_t c = 0; c < NUM_C; ++c) {
        doc_s[c] = std::min(total_docs, c ? ((c * dpc + 63) & ~63u) : 0u);
        if (c) doc_e[c - 1] = doc_s[c];
    }
    doc_e[NUM_C - 1] = total_docs;
    for (uint32_t c = 1; c < NUM_C; ++c) {
        int64_t lo = 0, hi = (int64_t)f_sz - 1, ans = f_sz;
        while (lo <= hi) {
            int64_t m = (lo + hi) >> 1, p = m;
            while (p < (int64_t)f_sz && p > 0 && d_buf[p - 1] != '\n') p++;
            if (p >= (int64_t)f_sz) { hi = m - 1; continue; }
            uint32_t id = 0; for (const char* cp = d_buf + p; cp < d_buf + f_sz && *cp >= '0' && *cp <= '9';) id = id * 10 + (*cp++ - '0');
            if (id >= doc_s[c]) { ans = p; hi = m - 1; } else lo = p + 1;
        }
        c_byte[c] = ans;
    }

    n_thr = std::min(NUM_C, std::thread::hardware_concurrency()); if (!n_thr) n_thr = 1;
    size_t sc_sz = (size_t)(NUM_C + n_thr) * n_terms * 4;
    uint32_t* sc = (uint32_t*)mmap(nullptr, sc_sz, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    {
        std::atomic<uint32_t> ci{0}; std::vector<std::thread> th;
        for (uint32_t t = 0; t < n_thr; ++t) th.emplace_back([&, t]() {
            uint32_t* ls = sc + (size_t)(NUM_C + t) * n_terms;
            while (true) { uint32_t c = ci.fetch_add(1, std::memory_order_relaxed); if (c >= NUM_C) break; pass1(c, d_buf, q_buf, sc + c * n_terms, ls); }
        });
        for (auto& w : th) w.join();
    }

    term_df.assign(n_terms, 0); is_bs.assign(n_terms, 0); bs_idx.assign(n_terms, UINT32_MAX);
    for (uint32_t c = 0; c < NUM_C; ++c) for (uint32_t i = 0; i < n_terms; ++i) term_df[i] += sc[c * n_terms + i];
    for (uint32_t i = 0; i < n_terms; ++i) if (term_df[i] > BS_TH) { is_bs[i] = 1; bs_idx[i] = n_bs++; }

    w_per_doc = (total_docs + 63) >> 6;
    g_bs = (uint64_t*)calloc((size_t)n_bs * w_per_doc, 8);
    uint32_t tot_u16 = 0;
    for (uint32_t c = 0; c < NUM_C; ++c) {
        c_base[c] = tot_u16; c_off[c].resize(n_terms);
        for (uint32_t i = 0; i < n_terms; ++i) { c_off[c][i] = tot_u16; if (!is_bs[i]) tot_u16 += sc[c * n_terms + i]; }
    }
    munmap(sc, sc_sz); // Immediate deallocation before allocating g_u16

    g_u16 = (uint16_t*)malloc((size_t)tot_u16 * 2);
    run_par(NUM_C, 1, [&](uint32_t c) { pass2(c, d_buf, q_buf); });

    uint32_t n_q = (uint32_t)queries.size();
    q_res.resize(n_q);
    res_buf = (char*)malloc((size_t)n_q * 48 + 65536);
    run_par(n_q, 64, [&](uint32_t i) {
        if (!queries[i].kind) exec_bool(queries[i], q_res[i]);
        else exec_phrase(queries[i], d_buf, q_buf, q_res[i]);
    });

    // Immediate stage deallocation before file I/O
    munmap((void*)d_buf, f_sz); munmap((void*)q_buf, q_sz);
    free(doc_ts); free(g_bs); free(g_u16);
    std::vector<Query>().swap(queries);

    int fd = open(rp, O_WRONLY | O_CREAT | O_TRUNC, 0666);
    char wbuf[1 << 16]; uint32_t wp = 0;
    for (uint32_t i = 0; i < n_q; ++i) {
        const auto& sl = q_res[i];
        if (wp + sl.len > sizeof(wbuf)) { ssize_t _ = write(fd, wbuf, wp); (void)_; wp = 0; }
        memcpy(wbuf + wp, res_buf + sl.off, sl.len); wp += sl.len;
    }
    if (wp) { ssize_t _ = write(fd, wbuf, wp); (void)_; }
    close(fd);
    return 0;
}
