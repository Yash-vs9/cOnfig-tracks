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

static constexpr uint32_t H_SZ = 131072, H_MSK = H_SZ - 1, NUM_C = 24, BS_TH = 768, MAX_W = 256;
alignas(64) static uint8_t cmap[256];
struct Slot { uint32_t h32, tid; } static vtab[H_SZ];
struct TInfo { uint64_t w0, w1; uint32_t off; uint16_t len; uint8_t in_phr; } static tinfo[81920];
alignas(64) static uint64_t bloom[8192];
static uint32_t n_terms = 0, total_docs = 0, doc_s[NUM_C], doc_e[NUM_C];
static size_t c_byte[NUM_C + 1];
struct Query { uint8_t kind, k; uint16_t ops; uint32_t tids[8]; };
static std::vector<Query> queries;
static std::vector<uint32_t> rep_map;
struct CRes { uint32_t cnt, top[10]; uint64_t sum; uint8_t tc; };
static CRes* all_res = nullptr;

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
    tinfo[tid] = {w0, w1, (uint32_t)(s - q_buf), (uint16_t)len, 0};
    bloom[(h >> 32) & 8191] |= 1ULL << (h & 63);
    return tid;
}

static void process_chunk(uint32_t c, int fd_docs, const char* q_buf) {
    uint32_t d0 = doc_s[c], c_docs = doc_e[c] - d0;
    if (!c_docs) return;
    uint32_t cw = (c_docs + 63) >> 6, r_sz = (uint32_t)rep_map.size(), n_bs = 0, tot_u16 = 0;
    std::vector<uint32_t> cnts(n_terms, 0), seen(n_terms, UINT32_MAX), off(n_terms), bs_id(n_terms, UINT32_MAX), doc_p_off(c_docs + 1, 0);
    std::vector<uint32_t> raw_post; raw_post.reserve(c_docs * 24);
    std::vector<uint64_t> phr_toks; phr_toks.reserve(c_docs * 8);

    // Stream chunk via pread into a 256 KB buffer -> 0 MB of docs.txt added to process VmRSS!
    constexpr size_t BUF_SZ = 1 << 18;
    char* buf = (char*)malloc(BUF_SZ + 64);
    size_t f_pos = c_byte[c], f_end = c_byte[c + 1], b_len = 0, b_pos = 0;

    auto refill = [&]() {
        size_t rem = b_len - b_pos;
        if (rem && b_pos) memmove(buf, buf + b_pos, rem);
        b_len = rem; b_pos = 0;
        if (f_pos < f_end) {
            size_t to_read = std::min(BUF_SZ - b_len, f_end - f_pos);
            ssize_t r = pread(fd_docs, buf + b_len, to_read, f_pos);
            if (r > 0) { b_len += r; f_pos += r; }
        }
        buf[b_len] = '\n';
    };
    refill();

    while (b_pos < b_len || f_pos < f_end) {
        if (b_len - b_pos < 65536 && f_pos < f_end) refill();
        if (b_pos >= b_len) break;
        const char* p = buf + b_pos;
        uint32_t d = 0;
        while (*p >= '0' && *p <= '9') d = d * 10 + (*p++ - '0');
        if (*p == '\t') p++;
        uint16_t ld = (uint16_t)(d - d0);
        doc_p_off[ld] = (uint32_t)phr_toks.size();
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
            uint32_t tid = ((bloom[(h >> 32) & 8191] >> (h & 63)) & 1ULL) ? lookup_swar(w0, w1, tail, len, q_buf, h) : UINT32_MAX;
            if (tid != UINT32_MAX) {
                if (seen[tid] != d) { seen[tid] = d; cnts[tid]++; raw_post.push_back((tid << 14) | ld); }
                if (tinfo[tid].in_phr) phr_toks.push_back(((uint64_t)pos << 32) | tid);
                if (__builtin_expect(p_tid == tid && pos == p_pos + 1, 0) && tid < r_sz && rep_map[tid] != UINT32_MAX) {
                    uint32_t rt = rep_map[tid];
                    if (seen[rt] != d) { seen[rt] = d; cnts[rt]++; raw_post.push_back((rt << 14) | ld); }
                }
                p_tid = tid; p_pos = pos;
            } else p_tid = UINT32_MAX;
            pos++;
        }
        doc_p_off[ld + 1] = (uint32_t)phr_toks.size();
        b_pos = (size_t)(p - buf);
    }
    free(buf);
    std::vector<uint32_t>().swap(seen);

    for (uint32_t i = 0; i < n_terms; ++i) {
        off[i] = tot_u16;
        if (cnts[i] > BS_TH) bs_id[i] = n_bs++;
        else tot_u16 += cnts[i];
    }
    std::vector<uint64_t> bs((size_t)n_bs * cw, 0);
    std::vector<uint16_t> u16(tot_u16);
    std::vector<uint32_t> cur_off = off;
    for (uint32_t rp : raw_post) {
        uint32_t tid = rp >> 14, ld = rp & 0x3FFF;
        if (bs_id[tid] != UINT32_MAX) bs[(size_t)bs_id[tid] * cw + (ld >> 6)] |= 1ULL << (ld & 63);
        else u16[cur_off[tid]++] = (uint16_t)ld;
    }
    std::vector<uint32_t>().swap(raw_post);
    std::vector<uint32_t>().swap(cur_off);

    auto get_bs = [&](uint32_t tid, uint64_t* out) {
        if (tid == UINT32_MAX || !cnts[tid]) { memset(out, 0, cw * 8); return; }
        if (bs_id[tid] != UINT32_MAX) memcpy(out, &bs[(size_t)bs_id[tid] * cw], cw * 8);
        else {
            memset(out, 0, cw * 8);
            for (uint32_t i = off[tid], e = i + cnts[tid]; i < e; ++i) out[u16[i] >> 6] |= 1ULL << (u16[i] & 63);
        }
    };

    uint32_t n_q = (uint32_t)queries.size();
    CRes* my_res = all_res + (size_t)c * n_q;
    uint16_t clist[BS_TH + 1], tmp[BS_TH + 1], cand[16384];
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
                        const uint64_t* b = &bs[(size_t)bs_id[nt] * cw];
                        if (op == 1) for (uint32_t w = 0; w < cw; ++w) curr[w] &= b[w];
                        else if (op == 2) for (uint32_t w = 0; w < cw; ++w) curr[w] |= b[w];
                        else for (uint32_t w = 0; w < cw; ++w) curr[w] &= ~b[w];
                    } else if (op == 2) for (uint32_t i = ns; i < ne; ++i) curr[u16[i] >> 6] |= 1ULL << (u16[i] & 63);
                    else if (op == 3) for (uint32_t i = ns; i < ne; ++i) curr[u16[i] >> 6] &= ~(1ULL << (u16[i] & 63));
                    else { get_bs(nt, nb); for (uint32_t w = 0; w < cw; ++w) curr[w] &= nb[w]; }
                } else if (op == 1) {
                    if (!clen) continue;
                    uint32_t w = 0;
                    if (bs_id[nt] != UINT32_MAX) {
                        const uint64_t* b = &bs[(size_t)bs_id[nt] * cw];
                        for (uint32_t i = 0; i < clen; ++i) if ((b[clist[i] >> 6] >> (clist[i] & 63)) & 1ULL) clist[w++] = clist[i];
                    } else for (uint32_t i = 0, j = ns; i < clen && j < ne;)
                        if (clist[i] == u16[j]) { clist[w++] = clist[i++]; j++; } else if (clist[i] < u16[j]) i++; else j++;
                    clen = w;
                } else if (op == 3) {
                    if (!clen) continue;
                    uint32_t w = 0;
                    if (bs_id[nt] != UINT32_MAX) {
                        const uint64_t* b = &bs[(size_t)bs_id[nt] * cw];
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
                for (uint32_t i = 0; i < clen; ++i) { uint32_t d = d0 + clist[i]; r.sum += d; if (r.tc < 10) r.top[r.tc++] = d; }
            } else {
                if (c_docs & 63) curr[cw - 1] &= (1ULL << (c_docs & 63)) - 1ULL;
                for (uint32_t w = 0; w < cw; ++w) {
                    uint64_t v = curr[w]; if (!v) continue;
                    r.cnt += __builtin_popcountll(v);
                    for (uint32_t b = d0 + (w << 6); v; v &= v - 1) {
                        uint32_t d = b + __builtin_ctzll(v); r.sum += d; if (r.tc < 10) r.top[r.tc++] = d;
                    }
                }
            }
        } else {
            uint8_t k = q.k, nd = 0; uint32_t dist[16]; bool ok = true;
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
                if (nd == 1) cptr = u16.data() + off[rare]; else memcpy(cand, u16.data() + off[rare], clen * 2);
            } else {
                const uint64_t* src = &bs[(size_t)bs_id[rare] * cw];
                for (uint32_t w = 0; w < cw; ++w)
                    for (uint64_t v = src[w]; v; v &= v - 1) { uint32_t id = (w << 6) + __builtin_ctzll(v); if (id < c_docs) cand[clen++] = (uint16_t)id; }
            }
            for (uint8_t ti = 1; ti < nd && clen; ++ti) {
                uint32_t ot = dist[ti], w = 0, ns = off[ot], ne = ns + cnts[ot];
                if (bs_id[ot] != UINT32_MAX) {
                    const uint64_t* b = &bs[(size_t)bs_id[ot] * cw];
                    for (uint32_t i = 0; i < clen; ++i) if ((b[cand[i] >> 6] >> (cand[i] & 63)) & 1ULL) cand[w++] = cand[i];
                } else for (uint32_t i = 0, j = ns; i < clen && j < ne;)
                    if (cand[i] == u16[j]) { cand[w++] = cand[i++]; j++; } else if (cand[i] < u16[j]) i++; else j++;
                clen = w;
            }
            // Verify phrase directly from L2-cached positional array (0 text scans!)
            uint32_t t0 = q.tids[0];
            for (uint32_t ci = 0; ci < clen; ++ci) {
                uint16_t ld = cptr[ci];
                if (k == 1) { uint32_t d = d0 + ld; r.cnt++; r.sum += d; if (r.tc < 10) r.top[r.tc++] = d; continue; }
                uint32_t ps = doc_p_off[ld], pe = doc_p_off[ld + 1];
                bool m = false;
                for (uint32_t pi = ps; pi + k <= pe; ++pi) {
                    uint64_t pk = phr_toks[pi];
                    if ((uint32_t)pk == t0) {
                        uint32_t base_pos = (uint32_t)(pk >> 32), j = 1;
                        for (; j < k; ++j)
                            if (phr_toks[pi + j] != (((uint64_t)(base_pos + j) << 32) | q.tids[j])) break;
                        if (j == k) { m = true; break; }
                    }
                }
                if (m) { uint32_t d = d0 + ld; r.cnt++; r.sum += d; if (r.tc < 10) r.top[r.tc++] = d; }
            }
        }
    }
}

int main(int argc, char** argv) {
    cmap['\n'] = 1;
    for (int i = '0'; i <= '9'; ++i) cmap[i] = i;
    for (int i = 'a'; i <= 'z'; ++i) cmap[i] = i, cmap[i - 32] = i;
    const char *d_dir = "/data", *o_dir = "/out";
    for (int i = 1; i + 1 < argc; i += 2) if (!strcmp(argv[i], "--data")) d_dir = argv[i + 1]; else if (!strcmp(argv[i], "--out")) o_dir = argv[i + 1];
    char dp[512], qp[512], rp[512];
    snprintf(dp, 512, "%s/docs.txt", d_dir); snprintf(qp, 512, "%s/queries.txt", d_dir); snprintf(rp, 512, "%s/results.txt", o_dir);

    int fd_q = open(qp, O_RDONLY), fd_d = open(dp, O_RDONLY);
    if (fd_q < 0 || fd_d < 0) return 1;
    struct stat st_q, st_d; fstat(fd_q, &st_q); fstat(fd_d, &st_d);
    size_t q_sz = st_q.st_size, f_sz = st_d.st_size;
    const char* q_buf = (const char*)mmap(nullptr, q_sz, PROT_READ, MAP_PRIVATE, fd_q, 0);
    close(fd_q);

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
            else {
                uint32_t tid = insert_term(tk, l, q_buf);
                if (!ib) tinfo[tid].in_phr = 1;
                q.tids[q.k++] = tid;
            }
        }
        queries.push_back(q);
        qp_ = (const char*)memchr(le, '\n', qe - le); if (qp_) qp_++; else break;
    }
    rep_map.assign(n_terms, UINT32_MAX);
    for (const auto& q : queries) if (q.kind) for (uint8_t i = 0; i + 1 < q.k; ++i)
        if (q.tids[i] == q.tids[i + 1] && rep_map[q.tids[i]] == UINT32_MAX) {
            uint32_t rt = n_terms++; rep_map[q.tids[i]] = rt; tinfo[rt] = tinfo[q.tids[i]];
        }

    char tail_buf[4096];
    size_t t_read = std::min((size_t)4096, f_sz);
    pread(fd_d, tail_buf, t_read, f_sz - t_read);
    const char* pl = tail_buf + t_read - 1;
    while (pl > tail_buf && *(pl - 1) != '\n') pl--;
    while (pl < tail_buf + t_read && *pl >= '0' && *pl <= '9') total_docs = total_docs * 10 + (*pl++ - '0');
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
        char probe[512];
        while (lo <= hi) {
            int64_t m = (lo + hi) >> 1;
            ssize_t r = pread(fd_d, probe, 512, m);
            if (r <= 0) { hi = m - 1; continue; }
            int64_t off_nl = 0;
            if (m > 0) { while (off_nl < r && probe[off_nl] != '\n') off_nl++; off_nl++; }
            if (off_nl >= r) { hi = m - 1; continue; }
            uint32_t id = 0; int64_t k = off_nl;
            while (k < r && probe[k] >= '0' && probe[k] <= '9') id = id * 10 + (probe[k++] - '0');
            if (id >= doc_s[c]) { ans = m + off_nl; hi = m - 1; } else lo = m + off_nl + 1;
        }
        c_byte[c] = ans;
    }

    uint32_t n_q = (uint32_t)queries.size(), n_thr = std::min(12u, std::thread::hardware_concurrency());
    all_res = (CRes*)calloc((size_t)NUM_C * n_q, sizeof(CRes));
    std::atomic<uint32_t> next_c{0};
    std::vector<std::thread> th;
    for (uint32_t t = 0; t < n_thr; ++t) th.emplace_back([&]() {
        while (true) { uint32_t c = next_c.fetch_add(1, std::memory_order_relaxed); if (c >= NUM_C) break; process_chunk(c, fd_d, q_buf); }
    });
    for (auto& w : th) w.join();
    close(fd_d); munmap((void*)q_buf, q_sz);

    int fd_o = open(rp, O_WRONLY | O_CREAT | O_TRUNC, 0666);
    char wbuf[1 << 16]; uint32_t wp = 0;
    for (uint32_t qi = 0; qi < n_q; ++qi) {
        uint64_t cnt = 0, sum = 0; uint32_t top[10], tc = 0;
        for (uint32_t c = 0; c < NUM_C; ++c) {
            const CRes& r = all_res[(size_t)c * n_q + qi];
            cnt += r.cnt; sum += r.sum;
            for (uint8_t i = 0; i < r.tc && tc < 10; ++i) top[tc++] = r.top[i];
        }
        if (wp > (1 << 16) - 256) { ssize_t _ = write(fd_o, wbuf, wp); (void)_; wp = 0; }
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
    if (wp) { ssize_t _ = write(fd_o, wbuf, wp); (void)_; }
    close(fd_o);
    return 0;
}
