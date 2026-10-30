/**
 * Speed Demon 2026 - Track D (Traverse)
 * Step 1: Native C++20 Forward CSR Baseline with 4-Ary Heap & Epoch Distance Tracking
 */

#include <fcntl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>
#include <algorithm>
#include <chrono>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <vector>

struct Edge {
    uint32_t to;
    uint32_t w;
};

struct HeapNode {
    uint64_t dist;
    uint32_t u;
};

// 4-ary min-heap: shallow tree (half the depth of binary heap) with 4 children per cache line
struct MinHeap4 {
    std::vector<HeapNode> data;

    void clear() {
        data.clear();
    }

    bool empty() const {
        return data.empty();
    }

    void push(uint64_t dist, uint32_t u) {
        data.push_back({dist, u});
        uint32_t i = data.size() - 1;
        while (i > 0) {
            uint32_t p = (i - 1) >> 2;
            if (data[i].dist < data[p].dist) {
                std::swap(data[i], data[p]);
                i = p;
            } else {
                break;
            }
        }
    }

    HeapNode pop() {
        HeapNode top = data[0];
        HeapNode last = data.back();
        data.pop_back();
        if (!data.empty()) {
            data[0] = last;
            uint32_t i = 0;
            uint32_t size = data.size();
            while (true) {
                uint32_t c = (i << 2) + 1;
                if (c >= size) break;
                uint32_t min_c = c;
                uint32_t end_c = std::min(c + 4, size);
                for (uint32_t k = c + 1; k < end_c; ++k) {
                    if (data[k].dist < data[min_c].dist) {
                        min_c = k;
                    }
                }
                if (data[min_c].dist < data[i].dist) {
                    std::swap(data[i], data[min_c]);
                    i = min_c;
                } else {
                    break;
                }
            }
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
            data = (char*)mmap(nullptr, size, PROT_READ, MAP_SHARED, fd, 0);
            madvise(data, size, MADV_SEQUENTIAL);
        }
    }

    ~FastMmap() {
        if (data && data != MAP_FAILED) munmap(data, size);
        if (fd >= 0) close(fd);
    }
};

int main(int argc, char** argv) {
    const char* data_dir = "/data";
    const char* out_dir = "/out";

    int pos = 0;
    for (int i = 1; i < argc; ++i) {
        if (strcmp(argv[i], "--data") == 0 && i + 1 < argc) {
            data_dir = argv[++i];
        } else if (strcmp(argv[i], "--out") == 0 && i + 1 < argc) {
            out_dir = argv[++i];
        } else if (argv[i][0] != '-') {
            if (pos == 0) data_dir = argv[i];
            else if (pos == 1) out_dir = argv[i];
            pos++;
        }
    }

    std::string graph_path = std::string(data_dir) + "/graph.txt";
    std::string queries_path = std::string(data_dir) + "/queries.txt";
    std::string out_path = std::string(out_dir) + "/distances.txt";

    // Fast mmap graph.txt
    FastMmap gmap(graph_path.c_str());
    if (!gmap.data) {
        fprintf(stderr, "Failed to open %s\n", graph_path.c_str());
        return 1;
    }

    const char* p = gmap.data;
    const char* g_end = gmap.data + gmap.size;

    // Parse header: n m
    uint32_t n = 0;
    while (*p >= '0') n = n * 10 + (*p++ - '0');
    p++; // ' '
    uint32_t m = 0;
    while (*p >= '0') m = m * 10 + (*p++ - '0');
    p++; // '\n'

    const char* edge_start = p;

    // Pass 1: Count out-degrees and mark nodes with incoming edges
    std::vector<uint32_t> out_deg(n, 0);
    std::vector<uint64_t> has_in_bitset((n + 63) / 64, 0);

    for (uint32_t i = 0; i < m; ++i) {
        uint32_t u = 0;
        while (*p >= '0') u = u * 10 + (*p++ - '0');
        p++;
        uint32_t v = 0;
        while (*p >= '0') v = v * 10 + (*p++ - '0');
        p++;
        while (*p >= '0') p++; // skip w
        p++; // skip '\n'

        if (u != v) {
            out_deg[u]++;
            has_in_bitset[v >> 6] |= (1ULL << (v & 63));
        }
    }

    // Build CSR head index
    std::vector<uint32_t> head(n + 1, 0);
    for (uint32_t i = 0; i < n; ++i) {
        head[i + 1] = head[i] + out_deg[i];
    }
    uint32_t total_edges = head[n];
    std::vector<Edge> edges(total_edges);

    // Pass 2: Populate edges directly
    std::vector<uint32_t> cur_pos = head;
    p = edge_start;
    for (uint32_t i = 0; i < m; ++i) {
        uint32_t u = 0;
        while (*p >= '0') u = u * 10 + (*p++ - '0');
        p++;
        uint32_t v = 0;
        while (*p >= '0') v = v * 10 + (*p++ - '0');
        p++;
        uint32_t w = 0;
        while (*p >= '0') w = w * 10 + (*p++ - '0');
        p++; // skip '\n'

        if (u != v) {
            edges[cur_pos[u]++] = {v, w};
        }
    }

    // Sort, deduplicate parallel edges, and compact in-place
    uint32_t compacted_total = 0;
    for (uint32_t u = 0; u < n; ++u) {
        uint32_t start = head[u];
        uint32_t e_end = head[u + 1];
        if (start < e_end) {
            std::sort(edges.data() + start, edges.data() + e_end, [](const Edge& a, const Edge& b) {
                if (a.to != b.to) return a.to < b.to;
                return a.w < b.w;
            });
        }
        uint32_t u_start = compacted_total;
        for (uint32_t i = start; i < e_end; ++i) {
            if (compacted_total > u_start && edges[i].to == edges[compacted_total - 1].to) {
                continue; // keep lightest
            }
            edges[compacted_total++] = edges[i];
        }
        head[u] = u_start;
    }
    head[n] = compacted_total;
    edges.resize(compacted_total);

    // Parse queries
    FastMmap qmap(queries_path.c_str());
    if (!qmap.data) {
        fprintf(stderr, "Failed to open %s\n", queries_path.c_str());
        return 1;
    }
    struct Query { uint32_t s, t; };
    std::vector<Query> queries;
    queries.reserve(2000);
    p = qmap.data;
    const char* q_end = qmap.data + qmap.size;
    while (p < q_end) {
        while (p < q_end && (*p == ' ' || *p == '\n' || *p == '\r')) p++;
        if (p >= q_end) break;
        uint32_t s = 0;
        while (p < q_end && *p >= '0') s = s * 10 + (*p++ - '0');
        if (p < q_end) p++; // ' '
        uint32_t t = 0;
        while (p < q_end && *p >= '0') t = t * 10 + (*p++ - '0');
        if (p < q_end) p++; // '\n'
        queries.push_back({s, t});
    }

    // Prepare Dijkstra structures
    std::vector<uint64_t> dist(n, UINT64_MAX);
    std::vector<uint32_t> visited_epoch(n, 0);
    uint32_t epoch = 0;
    MinHeap4 heap;
    heap.data.reserve(65536);

    std::vector<int64_t> answers(queries.size());

    // Execute queries
    for (size_t qi = 0; qi < queries.size(); ++qi) {
        uint32_t s = queries[qi].s;
        uint32_t t = queries[qi].t;

        if (s == t) {
            answers[qi] = 0;
            continue;
        }

        // Instant filter: s has no out edges OR t has no in edges
        if (head[s] == head[s + 1] || !(has_in_bitset[t >> 6] & (1ULL << (t & 63)))) {
            answers[qi] = -1;
            continue;
        }

        epoch++;
        heap.clear();
        dist[s] = 0;
        visited_epoch[s] = epoch;
        heap.push(0, s);

        int64_t ans = -1;
        while (!heap.empty()) {
            HeapNode top = heap.pop();
            uint64_t d = top.dist;
            uint32_t u = top.u;

            if (d > dist[u]) continue;
            if (u == t) {
                ans = d;
                break;
            }

            uint32_t start_e = head[u];
            uint32_t end_e = head[u + 1];
            for (uint32_t e = start_e; e < end_e; ++e) {
                uint32_t v = edges[e].to;
                uint64_t nd = d + edges[e].w;
                if (visited_epoch[v] != epoch || nd < dist[v]) {
                    visited_epoch[v] = epoch;
                    dist[v] = nd;
                    heap.push(nd, v);
                }
            }
        }
        answers[qi] = ans;
    }

    // Format output
    std::string out_buf;
    out_buf.reserve(queries.size() * 16);
    char num_buf[32];
    for (int64_t ans : answers) {
        if (ans < 0) {
            out_buf.append("-1\n");
        } else {
            int len = snprintf(num_buf, sizeof(num_buf), "%lu\n", (unsigned long)ans);
            out_buf.append(num_buf, len);
        }
    }

    // Atomic write to output file
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
