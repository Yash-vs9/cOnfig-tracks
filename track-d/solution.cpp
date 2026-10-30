/**
 * Speed Demon 2026 - Track D (Traverse)
 * Batch-Atomic CSR + Dataset Bucketing + 1-Bit Sink Filter + 8-Ary Packed Heap
 */

#include <fcntl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>
#include <algorithm>
#include <atomic>
#include <cstdint>
#include <cstring>
#include <string>
#include <thread>
#include <vector>

struct Edge {
    uint32_t to, w;
};

struct Query {
    uint32_t s, t, id;
};

struct SourceGroup {
    uint32_t s, begin_idx, end_idx;
};

// 8-ary MinHeap of packed 64-bit words: 8 children = 64 bytes (1 L1 cache line)
struct FastHeap8 {
    std::vector<uint64_t> data;

    inline void push(uint64_t packed) {
        uint32_t i = data.size();
        data.push_back(packed);
        while (i > 0) {
            uint32_t p = (i - 1) >> 3;
            if (packed >= data[p]) break;
            data[i] = data[p];
            i = p;
        }
        data[i] = packed;
    }

    inline uint64_t pop() {
        uint64_t top = data[0], last = data.back();
        data.pop_back();
        uint32_t sz = data.size(), i = 0;
        while (true) {
            uint32_t c = (i << 3) + 1;
            if (c >= sz) break;
            uint32_t mc = c, ec = std::min(c + 8, sz);
            for (uint32_t k = c + 1; k < ec; ++k) {
                if (data[k] < data[mc]) mc = k;
            }
            if (data[mc] >= last) break;
            data[i] = data[mc];
            i = mc;
        }
        if (sz > 0) data[i] = last;
        return top;
    }
};

struct FastMmap {
    int fd = -1;
    char* data = nullptr;
    size_t size = 0;

    FastMmap(const char* path) {
        fd = open(path, O_RDONLY);
        if (fd < 0) return;
        struct stat st;
        fstat(fd, &st);
        size = st.st_size;
        if (size > 0) {
            data = static_cast<char*>(mmap(nullptr, size, PROT_READ, MAP_PRIVATE, fd, 0));
            madvise(data, size, MADV_SEQUENTIAL);
        }
    }

    void release() {
        if (data && data != MAP_FAILED) munmap(data, size);
        if (fd >= 0) close(fd);
        data = nullptr;
        fd = -1;
    }

    ~FastMmap() { release(); }
};

template <typename F>
void run_parallel(uint32_t nt, F&& fn) {
    if (nt <= 1) {
        fn(0);
        return;
    }
    std::vector<std::thread> pool;
    pool.reserve(nt - 1);
    for (uint32_t t = 1; t < nt; ++t) pool.emplace_back(fn, t);
    fn(0);
    for (auto& w : pool) w.join();
}

int main(int argc, char** argv) {
    const char *data_dir = "/data", *out_dir = "/out";
    for (int i = 1, pos = 0; i < argc; ++i) {
        if (strcmp(argv[i], "--data") == 0 && i + 1 < argc) data_dir = argv[++i];
        else if (strcmp(argv[i], "--out") == 0 && i + 1 < argc) out_dir = argv[++i];
        else if (argv[i][0] != '-') (pos++ == 0 ? data_dir : out_dir) = argv[i];
    }

    FastMmap gmap((std::string(data_dir) + "/graph.txt").c_str());
    if (!gmap.data || gmap.data == MAP_FAILED) return 1;

    const char *p = gmap.data, *g_end = gmap.data + gmap.size;
    while (g_end > p && *(g_end - 1) < '0') g_end--;

    uint32_t n = 0, m = 0;
    while (p < g_end && *p >= '0') n = n * 10 + (*p++ - '0');
    while (p < g_end && *p < '0') p++;
    while (p < g_end && *p >= '0') m = m * 10 + (*p++ - '0');
    while (p < g_end && *p < '0') p++;

    const char* edge_start = p;
    size_t edge_bytes = g_end - edge_start;

    // Dataset scale bucketing: Tiny (<50k edges), Sample (50k-2M edges), Full (>=2M edges)
    const bool is_tiny = (m < 50000);
    const bool is_sample = (!is_tiny && m < 2000000);
    const uint32_t nt = is_tiny ? 1 : (is_sample ? 6 : 12);
    const uint32_t dijkstra_workers = is_tiny ? 1 : (is_sample ? 6 : 8);

    std::vector<const char*> chunk_bounds(nt + 1, g_end);
    chunk_bounds[0] = edge_start;
    for (uint32_t t = 1; t < nt; ++t) {
        const char* cp = edge_start + (edge_bytes * t) / nt;
        while (cp < g_end && *(cp - 1) != '\n') cp++;
        chunk_bounds[t] = cp;
    }

    std::vector<uint32_t> head(n + 1, 0);
    std::vector<uint64_t> has_in_bitset((n + 63) >> 6, 0);

    // Pass 1: Parallel degree counting with consecutive-u batching
    run_parallel(nt, [&](uint32_t t) {
        const char *cur = chunk_bounds[t], *end = chunk_bounds[t + 1];
        auto* deg_ptr = reinterpret_cast<std::atomic<uint32_t>*>(head.data());
        auto* bit_ptr = reinterpret_cast<std::atomic<uint64_t>*>(has_in_bitset.data());
        uint32_t prev_u = UINT32_MAX, run_cnt = 0;

        while (cur < end) {
            uint32_t u = *cur++ - '0';
            while (*cur >= '0') u = u * 10 + (*cur++ - '0');
            cur++;
            uint32_t v = *cur++ - '0';
            while (*cur >= '0') v = v * 10 + (*cur++ - '0');
            cur++;
            while (cur < end && *cur >= '0') cur++;
            if (cur < end && *cur == '\r') cur++;
            if (cur < end) cur++;

            if (u != v) {
                if (u == prev_u) {
                    run_cnt++;
                } else {
                    if (run_cnt) deg_ptr[prev_u].fetch_add(run_cnt, std::memory_order_relaxed);
                    prev_u = u;
                    run_cnt = 1;
                }
                uint64_t mask = 1ULL << (v & 63);
                if (!(bit_ptr[v >> 6].load(std::memory_order_relaxed) & mask)) {
                    bit_ptr[v >> 6].fetch_or(mask, std::memory_order_relaxed);
                }
            }
        }
        if (run_cnt) deg_ptr[prev_u].fetch_add(run_cnt, std::memory_order_relaxed);
    });

    uint32_t accum = 0;
    for (uint32_t i = 0; i <= n; ++i) {
        uint32_t d = head[i];
        head[i] = accum;
        accum += d;
    }

    std::vector<Edge> edges(accum);

    // Pass 2: In-place CSR edge population with consecutive-u batch reservation
    run_parallel(nt, [&](uint32_t t) {
        const char *cur = chunk_bounds[t], *end = chunk_bounds[t + 1];
        auto* pos_ptr = reinterpret_cast<std::atomic<uint32_t>*>(head.data());
        Edge buf[64];
        uint32_t prev_u = UINT32_MAX, cnt = 0;

        while (cur < end) {
            uint32_t u = *cur++ - '0';
            while (*cur >= '0') u = u * 10 + (*cur++ - '0');
            cur++;
            uint32_t v = *cur++ - '0';
            while (*cur >= '0') v = v * 10 + (*cur++ - '0');
            cur++;
            uint32_t w = *cur++ - '0';
            while (cur < end && *cur >= '0') w = w * 10 + (*cur++ - '0');
            if (cur < end && *cur == '\r') cur++;
            if (cur < end) cur++;

            if (u != v) {
                if (u == prev_u && cnt < 64) {
                    buf[cnt++] = {v, w};
                } else {
                    if (cnt) {
                        uint32_t idx = pos_ptr[prev_u].fetch_add(cnt, std::memory_order_relaxed);
                        memcpy(&edges[idx], buf, cnt * sizeof(Edge));
                    }
                    prev_u = u;
                    buf[0] = {v, w};
                    cnt = 1;
                }
            }
        }
        if (cnt) {
            uint32_t idx = pos_ptr[prev_u].fetch_add(cnt, std::memory_order_relaxed);
            memcpy(&edges[idx], buf, cnt * sizeof(Edge));
        }
    });
    gmap.release();
    chunk_bounds = std::vector<const char*>();

    if (n > 0) memmove(&head[1], &head[0], n * sizeof(uint32_t));
    head[0] = 0;

    // Fast register insertion-sort for small degrees + duplicate check
    std::atomic<uint32_t> next_chunk{0};
    std::atomic<bool> has_dups{false};
    run_parallel(nt, [&](uint32_t) {
        bool local_dups = false;
        const uint32_t csz = is_tiny ? 256 : 8192;
        while (true) {
            uint32_t u_base = next_chunk.fetch_add(csz, std::memory_order_relaxed);
            if (u_base >= n) break;
            uint32_t u_end = std::min(n, u_base + csz);
            for (uint32_t u = u_base; u < u_end; ++u) {
                uint32_t s = head[u], e = head[u + 1], len = e - s;
                if (len <= 1) continue;
                Edge* base = edges.data() + s;
                if (len <= 24) {
                    for (uint32_t i = 1; i < len; ++i) {
                        Edge key = base[i];
                        int32_t j = static_cast<int32_t>(i) - 1;
                        while (j >= 0 && (base[j].to > key.to || (base[j].to == key.to && base[j].w > key.w))) {
                            base[j + 1] = base[j];
                            j--;
                        }
                        base[j + 1] = key;
                    }
                } else {
                    std::sort(base, base + len, [](const Edge& a, const Edge& b) {
                        return (a.to != b.to) ? (a.to < b.to) : (a.w < b.w);
                    });
                }
                if (!local_dups) {
                    for (uint32_t i = 1; i < len; ++i) {
                        if (base[i].to == base[i - 1].to) {
                            local_dups = true;
                            break;
                        }
                    }
                }
            }
        }
        if (local_dups) has_dups.store(true, std::memory_order_relaxed);
    });

    if (has_dups.load(std::memory_order_relaxed)) {
        uint32_t compacted = 0;
        for (uint32_t u = 0; u < n; ++u) {
            uint32_t s = head[u], e = head[u + 1], u_start = compacted;
            for (uint32_t i = s; i < e; ++i) {
                if (compacted > u_start && edges[i].to == edges[compacted - 1].to) continue;
                edges[compacted++] = edges[i];
            }
            head[u] = u_start;
        }
        head[n] = compacted;
    }

    // Build 1-bit fast filter of vertices with out-degree > 0
    uint32_t num_sinks = 0;
    std::vector<uint64_t> has_out_bitset((n + 63) >> 6, 0);
    for (uint32_t u = 0; u < n; ++u) {
        if (head[u] < head[u + 1]) has_out_bitset[u >> 6] |= (1ULL << (u & 63));
        else num_sinks++;
    }
    // Enable sink filtering if sinks represent a non-negligible fraction of the graph (>= 1%)
    const bool use_sink_filter = (num_sinks * 100 >= n);

    // Parse queries
    FastMmap qmap((std::string(data_dir) + "/queries.txt").c_str());
    if (!qmap.data || qmap.data == MAP_FAILED) return 1;
    std::vector<Query> queries;
    queries.reserve(4096);
    p = qmap.data;
    const char* q_end = qmap.data + qmap.size;
    for (uint32_t qid = 0; p < q_end;) {
        while (p < q_end && *p < '0') p++;
        if (p >= q_end) break;
        uint32_t s = 0, t = 0;
        while (p < q_end && *p >= '0') s = s * 10 + (*p++ - '0');
        while (p < q_end && *p < '0') p++;
        while (p < q_end && *p >= '0') t = t * 10 + (*p++ - '0');
        queries.push_back({s, t, qid++});
    }
    qmap.release();

    std::vector<int64_t> answers(queries.size(), -1);
    std::vector<Query> active_q;
    active_q.reserve(queries.size());
    for (const auto& q : queries) {
        if (q.s == q.t) {
            answers[q.id] = 0;
        } else if (q.s < n && q.t < n && head[q.s] < head[q.s + 1] &&
                   (has_in_bitset[q.t >> 6] & (1ULL << (q.t & 63)))) {
            active_q.push_back(q);
        }
    }
    queries = std::vector<Query>();
    has_in_bitset = std::vector<uint64_t>();

    std::sort(active_q.begin(), active_q.end(), [](const Query& a, const Query& b) {
        return (a.s != b.s) ? (a.s < b.s) : (a.t < b.t);
    });

    std::vector<SourceGroup> groups;
    for (uint32_t i = 0, sz = active_q.size(); i < sz;) {
        uint32_t j = i + 1;
        while (j < sz && active_q[j].s == active_q[i].s) j++;
        groups.push_back({active_q[i].s, i, j});
        i = j;
    }
    std::sort(groups.begin(), groups.end(), [](const SourceGroup& a, const SourceGroup& b) {
        return (a.end_idx - a.begin_idx) > (b.end_idx - b.begin_idx);
    });

    uint32_t q_threads = std::max<uint32_t>(1, std::min<uint32_t>(dijkstra_workers, groups.size()));
    std::atomic<size_t> next_group{0};
    const uint32_t dirty_limit = is_tiny ? n : ((n >> 4) + 1024);
    const Edge* __restrict__ edge_ptr = edges.data();
    const uint32_t* __restrict__ head_ptr = head.data();
    const uint64_t* __restrict__ out_bit_ptr = has_out_bitset.data();

    constexpr uint32_t V_BITS = 22;
    constexpr uint64_t V_MASK = (1ULL << V_BITS) - 1;

    run_parallel(q_threads, [&](uint32_t) {
        std::vector<uint64_t> dist(n, UINT64_MAX);
        uint64_t* __restrict__ d_ptr = dist.data();
        std::vector<uint64_t> is_target((n + 63) >> 6, 0);
        std::vector<uint32_t> dirty;
        dirty.reserve(dirty_limit);
        FastHeap8 heap;
        heap.data.reserve(is_tiny ? 512 : 32768);

        while (true) {
            size_t gi = next_group.fetch_add(1, std::memory_order_relaxed);
            if (gi >= groups.size()) break;

            const auto& grp = groups[gi];
            uint32_t s = grp.s;
            uint32_t single_t = active_q[grp.begin_idx].t;
            const bool is_single = (single_t == active_q[grp.end_idx - 1].t);

            heap.data.clear();
            dirty.clear();
            bool dirty_overflow = false;

            d_ptr[s] = 0;
            dirty.push_back(s);
            heap.push(s);

            if (is_single) {
                // Dedicated Fast-Path for Single-Target Queries (zero target bitset pollution)
                uint64_t bound = UINT64_MAX;
                while (!heap.data.empty()) {
                    uint64_t top = heap.pop();
                    uint64_t d = top >> V_BITS;
                    uint32_t u = static_cast<uint32_t>(top & V_MASK);

                    if (d >= bound) break;
                    if (d > d_ptr[u]) continue;

                    uint32_t e_start = head_ptr[u], e_end = head_ptr[u + 1];
                    for (uint32_t e = e_start; e < e_end; ++e) {
                        if (e + 4 < e_end) __builtin_prefetch(&d_ptr[edge_ptr[e + 4].to], 0, 1);
                        uint32_t v = edge_ptr[e].to;
                        uint64_t nd = d + edge_ptr[e].w;
                        if (nd < d_ptr[v] && nd < bound) {
                            if (d_ptr[v] == UINT64_MAX && !dirty_overflow) {
                                if (dirty.size() < dirty_limit) dirty.push_back(v);
                                else dirty_overflow = true;
                            }
                            d_ptr[v] = nd;
                            if (v == single_t) {
                                bound = nd;
                            } else if (!use_sink_filter || (out_bit_ptr[v >> 6] & (1ULL << (v & 63)))) {
                                heap.push((nd << V_BITS) | v);
                            }
                        }
                    }
                }
                for (uint32_t k = grp.begin_idx; k < grp.end_idx; ++k) {
                    uint64_t d = d_ptr[single_t];
                    answers[active_q[k].id] = (d == UINT64_MAX) ? -1 : static_cast<int64_t>(d);
                }
            } else {
                // Multi-Target Group Path
                uint32_t remaining = 0;
                for (uint32_t k = grp.begin_idx; k < grp.end_idx; ++k) {
                    uint32_t tk = active_q[k].t;
                    uint64_t mask = 1ULL << (tk & 63);
                    if (!(is_target[tk >> 6] & mask)) {
                        is_target[tk >> 6] |= mask;
                        remaining++;
                    }
                }

                uint64_t bound = UINT64_MAX;
                while (!heap.data.empty()) {
                    uint64_t top = heap.pop();
                    uint64_t d = top >> V_BITS;
                    uint32_t u = static_cast<uint32_t>(top & V_MASK);

                    if (d >= bound) break;
                    if (d > d_ptr[u]) continue;

                    if (remaining > 1) {
                        uint64_t mask = 1ULL << (u & 63);
                        if (is_target[u >> 6] & mask) {
                            is_target[u >> 6] &= ~mask;
                            if (--remaining == 1) {
                                for (uint32_t k = grp.begin_idx; k < grp.end_idx; ++k) {
                                    uint32_t tk = active_q[k].t;
                                    if (is_target[tk >> 6] & (1ULL << (tk & 63))) {
                                        single_t = tk;
                                        is_target[tk >> 6] &= ~(1ULL << (tk & 63));
                                        bound = d_ptr[tk];
                                        break;
                                    }
                                }
                                if (d >= bound) break;
                            }
                        }
                    }

                    uint32_t e_start = head_ptr[u], e_end = head_ptr[u + 1];
                    for (uint32_t e = e_start; e < e_end; ++e) {
                        if (e + 4 < e_end) __builtin_prefetch(&d_ptr[edge_ptr[e + 4].to], 0, 1);
                        uint32_t v = edge_ptr[e].to;
                        uint64_t nd = d + edge_ptr[e].w;
                        if (nd < d_ptr[v] && nd < bound) {
                            if (d_ptr[v] == UINT64_MAX && !dirty_overflow) {
                                if (dirty.size() < dirty_limit) dirty.push_back(v);
                                else dirty_overflow = true;
                            }
                            d_ptr[v] = nd;
                            if (remaining == 1 && v == single_t) {
                                bound = nd;
                            } else if (!use_sink_filter || ((out_bit_ptr[v >> 6] | is_target[v >> 6]) & (1ULL << (v & 63)))) {
                                heap.push((nd << V_BITS) | v);
                            }
                        }
                    }
                }

                for (uint32_t k = grp.begin_idx; k < grp.end_idx; ++k) {
                    uint32_t tk = active_q[k].t;
                    if (remaining > 1) is_target[tk >> 6] = 0;
                    uint64_t d = d_ptr[tk];
                    answers[active_q[k].id] = (d == UINT64_MAX) ? -1 : static_cast<int64_t>(d);
                }
            }

            if (dirty_overflow) {
                std::fill(d_ptr, d_ptr + n, UINT64_MAX);
            } else {
                for (uint32_t u : dirty) d_ptr[u] = UINT64_MAX;
            }
        }
    });

    std::string out_buf;
    out_buf.reserve(answers.size() * 12);
    char tmp[24];
    for (int64_t ans : answers) {
        if (ans < 0) {
            out_buf.append("-1\n", 3);
        } else if (ans == 0) {
            out_buf.append("0\n", 2);
        } else {
            uint64_t val = static_cast<uint64_t>(ans);
            int idx = 23;
            tmp[idx--] = '\n';
            while (val > 0) {
                tmp[idx--] = '0' + (val % 10);
                val /= 10;
            }
            out_buf.append(tmp + idx + 1, 23 - idx);
        }
    }

    int out_fd = open((std::string(out_dir) + "/distances.txt").c_str(), O_WRONLY | O_CREAT | O_TRUNC, 0666);
    if (out_fd >= 0) {
        for (size_t written = 0; written < out_buf.size();) {
            ssize_t w = write(out_fd, out_buf.data() + written, out_buf.size() - written);
            if (w <= 0) break;
            written += w;
        }
        close(out_fd);
    }
    return 0;
}
