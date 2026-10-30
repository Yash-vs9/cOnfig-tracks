/**
 * Speed Demon 2026 - Track D (Traverse)
 * High-K Parallel CSR + Multi-Target Grouped Dijkstra + Zero-RSS-Overlap
 */

#include <fcntl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>
#include <algorithm>
#include <atomic>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <thread>
#include <vector>

struct Edge {
    uint32_t to;
    uint32_t w;
};

struct HeapNode {
    uint64_t dist;
    uint32_t u;
};

struct MinHeap4 {
    std::vector<HeapNode> data;

    void clear() { data.clear(); }
    bool empty() const { return data.empty(); }

    inline void push(uint64_t dist, uint32_t u) {
        uint32_t i = data.size();
        data.push_back({dist, u});
        while (i > 0) {
            uint32_t p = (i - 1) >> 2;
            if (dist < data[p].dist) {
                data[i] = data[p];
                i = p;
            } else {
                break;
            }
        }
        data[i] = {dist, u};
    }

    inline HeapNode pop() {
        HeapNode top = data[0];
        HeapNode last = data.back();
        data.pop_back();
        uint32_t size = data.size();
        if (size > 0) {
            uint32_t i = 0;
            while (true) {
                uint32_t c = (i << 2) + 1;
                if (c >= size) break;
                uint32_t min_c = c;
                uint32_t end_c = c + 4 < size ? c + 4 : size;
                for (uint32_t k = c + 1; k < end_c; ++k) {
                    if (data[k].dist < data[min_c].dist) min_c = k;
                }
                if (data[min_c].dist < last.dist) {
                    data[i] = data[min_c];
                    i = min_c;
                } else {
                    break;
                }
            }
            data[i] = last;
        }
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
            data = (char*)mmap(nullptr, size, PROT_READ, MAP_PRIVATE, fd, 0);
            madvise(data, size, MADV_SEQUENTIAL);
        }
    }

    void release() {
        if (data && data != MAP_FAILED) {
            munmap(data, size);
            data = nullptr;
        }
        if (fd >= 0) {
            close(fd);
            fd = -1;
        }
    }

    ~FastMmap() { release(); }
};

struct Query {
    uint32_t s, t, id;
};

int main(int argc, char** argv) {
    const char* data_dir = "/data";
    const char* out_dir = "/out";

    int pos = 0;
    for (int i = 1; i < argc; ++i) {
        if (strcmp(argv[i], "--data") == 0 && i + 1 < argc) data_dir = argv[++i];
        else if (strcmp(argv[i], "--out") == 0 && i + 1 < argc) out_dir = argv[++i];
        else if (argv[i][0] != '-') {
            if (pos == 0) data_dir = argv[i];
            else if (pos == 1) out_dir = argv[i];
            pos++;
        }
    }

    std::string graph_path = std::string(data_dir) + "/graph.txt";
    std::string queries_path = std::string(data_dir) + "/queries.txt";
    std::string out_path = std::string(out_dir) + "/distances.txt";

    FastMmap gmap(graph_path.c_str());
    if (!gmap.data) return 1;

    const char* p = gmap.data;
    const char* g_end = gmap.data + gmap.size;

    uint32_t n = 0, m = 0;
    while (*p >= '0') n = n * 10 + (*p++ - '0');
    p++;
    while (*p >= '0') m = m * 10 + (*p++ - '0');
    if (*p == '\r') p++;
    p++;

    const char* edge_start = p;
    size_t edge_bytes = g_end - edge_start;

    constexpr uint32_t NUM_THREADS = 12;
    uint32_t nt = (m < 50000) ? 1 : NUM_THREADS;

    // Partition file into line-aligned chunks for parallel parsing
    std::vector<const char*> chunk_bounds(nt + 1);
    chunk_bounds[0] = edge_start;
    chunk_bounds[nt] = g_end;
    for (uint32_t t = 1; t < nt; ++t) {
        const char* cp = edge_start + (edge_bytes * t) / nt;
        while (cp < g_end && *(cp - 1) != '\n') cp++;
        chunk_bounds[t] = cp;
    }

    std::vector<uint32_t> head(n + 1, 0);
    uint32_t bitset_words = (n + 63) >> 6;
    std::vector<uint64_t> has_in_bitset(bitset_words, 0);

    // Pass 1: Parallel degree counting & incoming bitset marking
    {
        std::vector<std::thread> workers;
        for (uint32_t t = 0; t < nt; ++t) {
            workers.emplace_back([&, t]() {
                const char* cur = chunk_bounds[t];
                const char* end = chunk_bounds[t + 1];
                auto* deg_ptr = reinterpret_cast<std::atomic<uint32_t>*>(head.data());
                auto* bit_ptr = reinterpret_cast<std::atomic<uint64_t>*>(has_in_bitset.data());

                while (cur < end) {
                    uint32_t u = 0;
                    while (*cur >= '0') u = u * 10 + (*cur++ - '0');
                    cur++;
                    uint32_t v = 0;
                    while (*cur >= '0') v = v * 10 + (*cur++ - '0');
                    cur++;
                    while (*cur >= '0') cur++;
                    if (*cur == '\r') cur++;
                    cur++;

                    if (u != v) {
                        deg_ptr[u].fetch_add(1, std::memory_order_relaxed);
                        uint64_t mask = 1ULL << (v & 63);
                        if ((bit_ptr[v >> 6].load(std::memory_order_relaxed) & mask) == 0) {
                            bit_ptr[v >> 6].fetch_or(mask, std::memory_order_relaxed);
                        }
                    }
                }
            });
        }
        for (auto& w : workers) w.join();
    }

    // Exclusive prefix sum in-place on head
    uint32_t accum = 0;
    for (uint32_t i = 0; i <= n; ++i) {
        uint32_t d = head[i];
        head[i] = accum;
        accum += d;
    }

    std::vector<Edge> edges(accum);

    // Pass 2: Parallel edge population
    {
        std::vector<uint32_t> cur_pos(head.begin(), head.begin() + n);
        std::vector<std::thread> workers;
        for (uint32_t t = 0; t < nt; ++t) {
            workers.emplace_back([&, t]() {
                const char* cur = chunk_bounds[t];
                const char* end = chunk_bounds[t + 1];
                auto* pos_ptr = reinterpret_cast<std::atomic<uint32_t>*>(cur_pos.data());

                while (cur < end) {
                    uint32_t u = 0;
                    while (*cur >= '0') u = u * 10 + (*cur++ - '0');
                    cur++;
                    uint32_t v = 0;
                    while (*cur >= '0') v = v * 10 + (*cur++ - '0');
                    cur++;
                    uint32_t w = 0;
                    while (*cur >= '0') w = w * 10 + (*cur++ - '0');
                    if (*cur == '\r') cur++;
                    cur++;

                    if (u != v) {
                        uint32_t idx = pos_ptr[u].fetch_add(1, std::memory_order_relaxed);
                        edges[idx] = {v, w};
                    }
                }
            });
        }
        for (auto& w : workers) w.join();
    }

    // Immediately unmap graph.txt to drop file-backed pages from RSS before query phase
    gmap.release();

    // Sort adjacency lists in parallel (sort by weight ascending first for better cache prefetching)
    {
        std::atomic<uint32_t> next_chunk{0};
        std::vector<std::thread> workers;
        for (uint32_t t = 0; t < nt; ++t) {
            workers.emplace_back([&]() {
                while (true) {
                    uint32_t u_base = next_chunk.fetch_add(4096, std::memory_order_relaxed);
                    if (u_base >= n) break;
                    uint32_t u_end = std::min(n, u_base + 4096);
                    for (uint32_t u = u_base; u < u_end; ++u) {
                        uint32_t s = head[u], e = head[u + 1];
                        if (e - s > 1) {
                            std::sort(edges.data() + s, edges.data() + e, [](const Edge& a, const Edge& b) {
                                return (a.to != b.to) ? (a.to < b.to) : (a.w < b.w);
                            });
                        }
                    }
                }
            });
        }
        for (auto& w : workers) w.join();
    }

    // Deduplicate parallel edges sequentially in-place
    uint32_t compacted = 0;
    for (uint32_t u = 0; u < n; ++u) {
        uint32_t s = head[u], e = head[u + 1];
        uint32_t u_start = compacted;
        for (uint32_t i = s; i < e; ++i) {
            if (compacted > u_start && edges[i].to == edges[compacted - 1].to) continue;
            edges[compacted++] = edges[i];
        }
        head[u] = u_start;
    }
    head[n] = compacted;

    // Parse queries
    FastMmap qmap(queries_path.c_str());
    if (!qmap.data) return 1;
    std::vector<Query> queries;
    queries.reserve(4096);
    p = qmap.data;
    const char* q_end = qmap.data + qmap.size;
    uint32_t qid = 0;
    while (p < q_end) {
        while (p < q_end && *p < '0') p++;
        if (p >= q_end) break;
        uint32_t s = 0;
        while (p < q_end && *p >= '0') s = s * 10 + (*p++ - '0');
        while (p < q_end && *p < '0') p++;
        uint32_t t = 0;
        while (p < q_end && *p >= '0') t = t * 10 + (*p++ - '0');
        queries.push_back({s, t, qid++});
    }
    qmap.release();

    size_t num_q = queries.size();
    std::vector<int64_t> answers(num_q, -1);

    // Filter trivial queries & group remaining queries by source vertex `s`
    std::vector<Query> active_q;
    active_q.reserve(num_q);
    for (const auto& q : queries) {
        if (q.s == q.t) {
            answers[q.id] = 0;
        } else if (head[q.s] == head[q.s + 1] || !(has_in_bitset[q.t >> 6] & (1ULL << (q.t & 63)))) {
            answers[q.id] = -1;
        } else {
            active_q.push_back(q);
        }
    }

    std::sort(active_q.begin(), active_q.end(), [](const Query& a, const Query& b) {
        return (a.s != b.s) ? (a.s < b.s) : (a.t < b.t);
    });

    struct SourceGroup {
        uint32_t s;
        uint32_t begin_idx;
        uint32_t end_idx;
    };
    std::vector<SourceGroup> groups;
    for (uint32_t i = 0; i < active_q.size();) {
        uint32_t j = i + 1;
        while (j < active_q.size() && active_q[j].s == active_q[i].s) j++;
        groups.push_back({active_q[i].s, i, j});
        i = j;
    }

    // Parallel Multi-Target Dijkstra Execution
    std::atomic<size_t> next_group{0};
    uint32_t q_threads = std::min<uint32_t>(NUM_THREADS, groups.size());
    if (q_threads == 0) q_threads = 1;

    {
        std::vector<std::thread> workers;
        for (uint32_t t = 0; t < q_threads; ++t) {
            workers.emplace_back([&]() {
                std::vector<uint64_t> dist(n, UINT64_MAX);
                std::vector<uint8_t> is_target(n, 0);
                std::vector<uint32_t> dirty;
                dirty.reserve(65536);

                MinHeap4 heap;
                heap.data.reserve(65536);

                while (true) {
                    size_t gi = next_group.fetch_add(1, std::memory_order_relaxed);
                    if (gi >= groups.size()) break;

                    const auto& grp = groups[gi];
                    uint32_t s = grp.s;
                    uint32_t remaining = 0;

                    for (uint32_t k = grp.begin_idx; k < grp.end_idx; ++k) {
                        uint32_t target = active_q[k].t;
                        if (!is_target[target]) {
                            is_target[target] = 1;
                            remaining++;
                        }
                    }

                    heap.clear();
                    dirty.clear();

                    dist[s] = 0;
                    dirty.push_back(s);
                    heap.push(0, s);

                    while (!heap.empty()) {
                        HeapNode top = heap.pop();
                        uint64_t d = top.dist;
                        uint32_t u = top.u;

                        if (d > dist[u]) continue;

                        if (is_target[u]) {
                            is_target[u] = 0;
                            if (--remaining == 0) break;
                        }

                        uint32_t e_start = head[u];
                        uint32_t e_end = head[u + 1];
                        for (uint32_t e = e_start; e < e_end; ++e) {
                            uint32_t v = edges[e].to;
                            uint64_t nd = d + edges[e].w;
                            if (nd < dist[v]) {
                                if (dist[v] == UINT64_MAX) dirty.push_back(v);
                                dist[v] = nd;
                                heap.push(nd, v);
                            }
                        }
                    }

                    for (uint32_t k = grp.begin_idx; k < grp.end_idx; ++k) {
                        uint32_t target = active_q[k].t;
                        is_target[target] = 0;
                        uint64_t d = dist[target];
                        answers[active_q[k].id] = (d == UINT64_MAX) ? -1 : static_cast<int64_t>(d);
                    }

                    // Fast sparse reset of touched vertices only
                    for (uint32_t u : dirty) {
                        dist[u] = UINT64_MAX;
                    }
                }
            });
        }
        for (auto& w : workers) w.join();
    }

    // Fast integer-to-ASCII output formatting
    std::string out_buf;
    out_buf.reserve(num_q * 12);
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

    int out_fd = open(out_path.c_str(), O_WRONLY | O_CREAT | O_TRUNC, 0666);
    if (out_fd >= 0) {
        size_t written = 0;
        while (written < out_buf.size()) {
            ssize_t w = write(out_fd, out_buf.data() + written, out_buf.size() - written);
            if (w <= 0) break;
            written += w;
        }
        close(out_fd);
    }

    return 0;
}
