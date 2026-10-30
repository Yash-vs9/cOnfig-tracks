#pragma GCC optimize("O3,unroll-loops")
#pragma GCC target("avx2,bmi,bmi2,lzcnt,popcnt")
#include <fcntl.h>
#include <immintrin.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>
#include <algorithm>
#include <charconv>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <thread>
#include <vector>

static constexpr uint32_t H_SZ = 131072, H_MSK = H_SZ - 1, NUM_C = 12, BS_TH = 1024, MAX_W = 512;
alignas(64) static uint8_t cmap[256];
struct Slot { uint32_t h32, tid; } static vtab[H_SZ];
struct TInfo { uint64_t w0, w1; uint32_t off; uint16_t len; uint8_t c0; } static tinfo[81920];
alignas(64) static uint64_t bloom[8192];
static uint32_t n_terms = 0, total_docs = 0, doc_s[NUM_C], doc_e[NUM_C];
static size_t c_byte[NUM_C + 1];
struct Query { uint8_t kind, k; uint16_t ops; uint32_t tids[8]; };
static std::vector<Query> queries;
static std::vector<uint32_t> rep_map;
struct CRes { uint32_t cnt, top[10]; uint64_t sum; uint8_t t_cnt; };
static CRes* all_res = nullptr;

static inline uint64_t mix64(uint64_t w0, uint64_t w1, uint32_t len) {
    uint64_t h = (w0 ^ ((w1 * 0x9e3779b97f4a7c15ULL) + len)) * 0xbf58476d1ce4e5b9ULL;
    return h ^ (h >> 31);
}

static inline uint32_t lookup_term(uint64_t w0, uint64_t w1, const char* tail, uint32_t len, const char* q_buf, uint64_t h) {
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
    uint32_t tid = lookup_term(w0, w1, s + 16, len, q_buf, h);
    if (tid != UINT32_MAX) return tid;
    uint32_t h32 = (uint32_t)(h ^ (h >> 32)) | 1u, idx = (uint32_t)h & H_MSK;
    while (vtab[idx].h32) idx = (idx + 1) & H_MSK;
    tid = n_terms++;
    vtab[idx] = {h32, tid};
    tinfo[tid] = {w0, w1, (uint32_t)(s - q_buf), (uint16_t)len, (uint8_t)s[0]};
    bloom[(h >> 32) & 8191] |= 1ULL << (h & 63);
    return tid;
}

template <typename F>
static inline void scan_doc(const char*& p, const char* q_buf, F&& fn) {
    uint32_t pos = 0, p_tid = UINT32_MAX, p_pos = UINT32_MAX;
    while (true) {
        while (!cmap[(uint8_t)*p]) p++;
        if (*p == '\n') { p++; break; }
        uint64_t w0 = 0, w1 = 0; char tail[32]; uint32_t len = 0;
        uint8_t m = cmap[(uint8_t)*p];
        do {
            if (len < 8) w0 |= (uint64_t)m << (len * 8);
            else if (len < 16) w1 |= (uint64_t)m << ((len - 8) * 8);
            else if (len < 48) tail[len - 16] = (char)m;
            len++; m = cmap[(uint8_t)*++p];
        } while (m > 1);
        uint64_t h = mix64(w0, w1, len);
        uint32_t tid = ((bloom[(h >> 32) & 8191] >> (h & 63)) & 1ULL) ? lookup_term(w0, w1, tail, len, q_buf, h) : UINT32_MAX;
        if (tid != UINT32_MAX) { fn(tid, p_tid == tid && pos == p_pos + 1); p_tid = tid; p_pos = pos; }
        else p_tid = UINT32_MAX;
        pos++;
    }
}

static void process_chunk(uint32_t c, const char* d_buf, const char* q_buf) {
    uint32_t d0 = doc_s[c], c_docs = doc_e[c] - d0;
    if (!c_docs) return;
    uint32_t c_words = (c_docs + 63) >> 6, r_sz = (uint32_t)rep_map.size(), n_bs = 0, tot_u16 = 0;
    std::vector<uint32_t> cnts(n_terms, 0), seen(n_terms, UINT32_MAX), doc_off(c_docs), off(n_terms), bs_id(n_terms, UINT32_MAX);
    const char *p = d_buf + c_byte[c], *pe = d_buf + c_byte[c + 1];
    while (p < pe) {
        uint32_t d = 0; while (*p >= '0' && *p <= '9') d = d * 10 + (*p++ - '0');
        if (*p == '\t') p++;
        doc_off[d - d0] = (uint32_t)(p - d_buf);
        scan_doc(p, q_buf, [&](uint32_t tid, bool rep) {
            if (seen[tid] != d) { cnts[tid]++; seen[tid] = d; }
            if (__builtin_expect(rep, 0) && tid < r_sz && rep_map[tid] != UINT32_MAX && seen[rep_map[tid]] != d) {
                cnts[rep_map[tid]]++; seen[rep_map[tid]] = d;
            }
        });
    }
    for (uint32_t i = 0; i < n_terms; ++i) {
        off[i] = tot_u16;
        if (cnts[i] > BS_TH) bs_id[i] = n_bs++;
        else tot_u16 += cnts[i];
    }
    std::vector<uint64_t> bs((size_t)n_bs * c_words, 0);
    std::vector<uint16_t> u16(tot_u16);
    std::vector<uint32_t> cur_off = off;
    std::fill(seen.begin(), seen.end(), UINT32_MAX);
    p = d_buf + c_byte[c];
    auto emit = [&](uint32_t tid, uint16_t ld) {
        if (bs_id[tid] != UINT32_MAX) bs[(size_t)bs_id[tid] * c_words + (ld >> 6)] |= 1ULL << (ld & 63);
        else u16[cur_off[tid]++] = ld;
    };
    while (p < pe) {
        uint32_t d = 0; while (*p >= '0' && *p <= '9') d = d * 10 + (*p++ - '0');
        if (*p == '\t') p++;
        uint16_t ld = (uint16_t)(d - d0);
        scan_doc(p, q_buf, [&](uint32_t tid, bool rep) {
            if (seen[tid] != d) { seen[tid] = d; emit(tid, ld); }
            if (__builtin_expect(rep, 0) && tid < r_sz && rep_map[tid] != UINT32_MAX && seen[rep_map[tid]] != d) {
                seen[rep_map[tid]] = d; emit(rep_map[tid], ld);
            }
        });
    }
    auto get_bs = [&](uint32_t tid, uint64_t* out) {
        if (tid == UINT32_MAX || !cnts[tid]) { memset(out, 0, c_words * 8); return; }
        if (bs_id[tid] != UINT32_MAX) memcpy(out, &bs[(size_t)bs_id[tid] * c_words], c_words * 8);
        else {
            memset(out, 0, c_words * 8);
            for (uint32_t i = off[tid], e = off[tid] + cnts[tid]; i < e; ++i) out[u16[i] >> 6] |= 1ULL << (u16[i] & 63);
        }
    };
    uint32_t n_q = (uint32_t)queries.size();
    CRes* my_res = all_res + (size_t)c * n_q;
    uint16_t clist[BS_TH + 1], tmp[BS_TH + 1], cand[21000];
    alignas(32) uint64_t curr[MAX_W], nb[MAX_W];

    for (uint32_t qi = 0; qi < n_q; ++qi) {
        const auto& q = queries[qi];
        CRes& r = my_res[qi];
        if (q.kind == 0) {
            uint32_t t0 = q.tids[0], clen = cnts[t0];
            bool in_bs = (bs_id[t0] != UINT32_MAX);
            if (in_bs) get_bs(t0, curr);
            else memcpy(clist, u16.data() + off[t0], clen * 2);
            for (uint8_t oi = 0; oi < q.k - 1; ++oi) {
                uint8_t op = (q.ops >> (oi * 2)) & 3;
                uint32_t nt = q.tids[oi + 1], ns = off[nt], ne = ns + cnts[nt];
                if (in_bs) {
                    if (bs_id[nt] != UINT32_MAX) {
                        const uint64_t* b = &bs[(size_t)bs_id[nt] * c_words];
                        if (op == 1) for (uint32_t w = 0; w < c_words; ++w) curr[w] &= b[w];
                        else if (op == 2) for (uint32_t w = 0; w < c_words; ++w) curr[w] |= b[w];
                        else for (uint32_t w = 0; w < c_words; ++w) curr[w] &= ~b[w];
                    } else if (op == 2) for (uint32_t i = ns; i < ne; ++i) curr[u16[i] >> 6] |= 1ULL << (u16[i] & 63);
                    else if (op == 3) for (uint32_t i = ns; i < ne; ++i) curr[u16[i] >> 6] &= ~(1ULL << (u16[i] & 63));
                    else { get_bs(nt, nb); for (uint32_t w = 0; w < c_words; ++w) curr[w] &= nb[w]; }
                } else if (op == 1) {
                    if (!clen) continue;
                    uint32_t w = 0;
                    if (bs_id[nt] != UINT32_MAX) {
                        const uint64_t* b = &bs[(size_t)bs_id[nt] * c_words];
                        for (uint32_t i = 0; i < clen; ++i) if ((b[clist[i] >> 6] >> (clist[i] & 63)) & 1ULL) clist[w++] = clist[i];
                    } else {
                        for (uint32_t i = 0, j = ns; i < clen && j < ne;)
                            if (clist[i] == u16[j]) { clist[w++] = clist[i++]; j++; } else if (clist[i] < u16[j]) i++; else j++;
                    }
                    clen = w;
                } else if (op == 3) {
                    if (!clen) continue;
                    uint32_t w = 0;
                    if (bs_id[nt] != UINT32_MAX) {
                        const uint64_t* b = &bs[(size_t)bs_id[nt] * c_words];
                        for (uint32_t i = 0; i < clen; ++i) if (!((b[clist[i] >> 6] >> (clist[i] & 63)) & 1ULL)) clist[w++] = clist[i];
                    } else {
                        uint32_t i = 0, j = ns;
                        while (i < clen && j < ne) if (clist[i] == u16[j]) { i++; j++; } else if (clist[i] < u16[j]) clist[w++] = clist[i++]; else j++;
                        while (i < clen) clist[w++] = clist[i++];
                    }
                    clen = w;
                } else {
                    if (bs_id[nt] != UINT32_MAX || clen + cnts[nt] > BS_TH) {
                        get_bs(nt, curr);
                        for (uint32_t i = 0; i < clen; ++i) curr[clist[i] >> 6] |= 1ULL << (clist[i] & 63);
                        in_bs = true;
                    } else {
                        uint32_t i = 0, j = ns, w = 0;
                        while (i < clen && j < ne) if (clist[i] == u16[j]) { tmp[w++] = clist[i++]; j++; } else if (clist[i] < u16[j]) tmp[w++] = clist[i++]; else tmp[w++] = u16[j++];
                        while (i < clen) tmp[w++] = clist[i++];
                        while (j < ne) tmp[w++] = u16[j++];
                        memcpy(clist, tmp, w * 2); clen = w;
                    }
                }
            }
            if (!in_bs) {
                r.cnt = clen;
                for (uint32_t i = 0; i < clen; ++i) { uint32_t d = d0 + clist[i]; r.sum += d; if (r.t_cnt < 10) r.top[r.t_cnt++] = d; }
            } else {
                if (c_docs & 63) curr[c_words - 1] &= (1ULL << (c_docs & 63)) - 1ULL;
                for (uint32_t w = 0; w < c_words; ++w) {
                    uint64_t v = curr[w]; if (!v) continue;
                    r.cnt += __builtin_popcountll(v);
                    for (uint32_t b = d0 + (w << 6); v; v &= v - 1) {
                        uint32_t d = b + __builtin_ctzll(v); r.sum += d; if (r.t_cnt < 10) r.top[r.t_cnt++] = d;
                    }
                }
            }
        } else {
            uint8_t k = q.k, nd = 0; uint32_t dist[16];
            bool ok = true;
            for (uint8_t i = 0; i < k; ++i) {
                uint32_t t = q.tids[i]; if (!cnts[t]) { ok = false; break; }
                if (std::find(dist, dist + nd, t) == dist + nd) dist[nd++] = t;
                if (i + 1 < k && t == q.tids[i + 1] && t < r_sz && rep_map[t] != UINT32_MAX) {
                    uint32_t rt = rep_map[t]; if (!cnts[rt]) { ok = false; break; }
                    if (std::find(dist, dist + nd, rt) == dist + nd) dist[nd++] = rt;
                }
            }
            if (!ok) continue;
            std::sort(dist, dist + nd, [&](uint32_t a, uint32_t b) { return cnts[a] < cnts[b]; });
            uint32_t rare = dist[0], clen = 0; const uint16_t* cptr = cand;
            if (bs_id[rare] == UINT32_MAX) {
                clen = cnts[rare];
                if (nd == 1) cptr = u16.data() + off[rare];
                else memcpy(cand, u16.data() + off[rare], clen * 2);
            } else {
                const uint64_t* src = &bs[(size_t)bs_id[rare] * c_words];
                for (uint32_t w = 0; w < c_words; ++w)
                    for (uint64_t v = src[w]; v; v &= v - 1) { uint32_t id = (w << 6) + __builtin_ctzll(v); if (id < c_docs) cand[clen++] = (uint16_t)id; }
            }
            for (uint8_t ti = 1; ti < nd && clen; ++ti) {
                uint32_t ot = dist[ti], w = 0, ns = off[ot], ne = ns + cnts[ot];
                if (bs_id[ot] != UINT32_MAX) {
                    const uint64_t* b = &bs[(size_t)bs_id[ot] * c_words];
                    for (uint32_t i = 0; i < clen; ++i) if ((b[cand[i] >> 6] >> (cand[i] & 63)) & 1ULL) cand[w++] = cand[i];
                } else {
                    for (uint32_t i = 0, j = ns; i < clen && j < ne;)
                        if (cand[i] == u16[j]) { cand[w++] = cand[i++]; j++; } else if (cand[i] < u16[j]) i++; else j++;
                }
                clen = w;
            }
            TInfo pt[8]; for (uint8_t i = 0; i < k; ++i) pt[i] = tinfo[q.tids[i]];
            auto eq = [&](const char* s, uint32_t l, const TInfo& ti) {
                if (l != ti.len) return false;
                const char* qb = q_buf + ti.off;
                for (uint32_t i = 0; i < l; ++i) if (cmap[(uint8_t)s[i]] != (uint8_t)qb[i]) return false;
                return true;
            };
            for (uint32_t ci = 0; ci < clen; ++ci) {
                const char* sp = d_buf + doc_off[cptr[ci]]; bool m = false;
                while (true) {
                    while (!cmap[(uint8_t)*sp]) sp++;
                    if (*sp == '\n') break;
                    const char* ts = sp; uint8_t fc = cmap[(uint8_t)*sp++];
                    while (cmap[(uint8_t)*sp] > 1) sp++;
                    if ((uint32_t)(sp - ts) == pt[0].len && fc == pt[0].c0 && eq(ts, pt[0].len, pt[0])) {
                        const char* xp = sp; uint8_t j = 1;
                        for (; j < k; ++j) {
                            while (!cmap[(uint8_t)*xp]) xp++;
                            if (*xp == '\n') break;
                            const char* js = xp; uint8_t jc = cmap[(uint8_t)*xp++];
                            while (cmap[(uint8_t)*xp] > 1) xp++;
                            if ((uint32_t)(xp - js) != pt[j].len || jc != pt[j].c0 || !eq(js, pt[j].len, pt[j])) break;
                        }
                        if (j == k) { m = true; break; }
                    }
                }
                if (m) { uint32_t d = d0 + cptr[ci]; r.cnt++; r.sum += d; if (r.t_cnt < 10) r.top[r.t_cnt++] = d; }
            }
        }
    }
    size_t pg_s = c_byte[c] & ~4095ULL, pg_e = c_byte[c + 1] & ~4095ULL;
    if (pg_e > pg_s) madvise((void*)(d_buf + pg_s), pg_e - pg_s, MADV_DONTNEED);
}

static const char* map_f(const char* path, size_t& sz) {
    int fd = open(path, O_RDONLY); if (fd < 0) return nullptr;
    struct stat st; fstat(fd, &st); sz = st.st_size;
    const char* p = (const char*)mmap(nullptr, sz, PROT_READ, MAP_PRIVATE, fd, 0);
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

    uint32_t n_q = (uint32_t)queries.size();
    all_res = (CRes*)calloc((size_t)NUM_C * n_q, sizeof(CRes));
    std::vector<std::thread> th;
    for (uint32_t c = 0; c < NUM_C; ++c) th.emplace_back(process_chunk, c, d_buf, q_buf);
    for (auto& w : th) w.join();

    int fd = open(rp, O_WRONLY | O_CREAT | O_TRUNC, 0666);
    char wbuf[1 << 16]; uint32_t wp = 0;
    for (uint32_t qi = 0; qi < n_q; ++qi) {
        uint64_t cnt = 0, sum = 0; uint32_t top[10], tc = 0;
        for (uint32_t c = 0; c < NUM_C; ++c) {
            const CRes& r = all_res[(size_t)c * n_q + qi];
            cnt += r.cnt; sum += r.sum;
            for (uint8_t i = 0; i < r.t_cnt && tc < 10; ++i) top[tc++] = r.top[i];
        }
        if (wp > (1 << 16) - 256) { ssize_t _ = write(fd, wbuf, wp); (void)_; wp = 0; }
        char* p = wbuf + wp;
        if (!cnt) { memcpy(p, "0 0\n", 4); p += 4; }
        else {
            p = std::to_chars(p, p + 32, cnt).ptr; *p++ = ' ';
            p = std::to_chars(p, p + 32, sum).ptr;
            for (uint32_t i = 0; i < tc; ++i) { *p++ = ' '; p = std::to_chars(p, p + 16, top[i]).ptr; }
            *p++ = '\n';
        }
        wp = (uint32_t)(p - wbuf);
    }
    if (wp) { ssize_t _ = write(fd, wbuf, wp); (void)_; }
    close(fd);
    return 0;
}
