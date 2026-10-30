/**
 * Speed Demon 2026 - Track B (Index)
 * Elite Zero-Dependency C++20 Solution
 */
#include <fcntl.h>
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

static constexpr uint32_t HASH_SIZE = 131072, HASH_MASK = HASH_SIZE - 1;
static constexpr uint32_t BITSET_THRESHOLD = 15625, NUM_CHUNKS = 12, MAX_CHUNK_WORDS = 384;

alignas(64) static uint8_t char_map[256], char_map_sep[256];
static void init_char_map() {
    for (int i = 0; i < 256; ++i) {
        char_map_sep[i] = (i != '\n' && i != 0);
        char_map[i] = (i >= '0' && i <= '9') || (i >= 'a' && i <= 'z') ? (uint8_t)i :
                      (i >= 'A' && i <= 'Z') ? (uint8_t)(i + 32) : 0;
        if (char_map[i]) char_map_sep[i] = 0;
    }
}

struct VocabSlot { uint32_t hash32, term_id; };
struct TermInfo { uint32_t str_off; uint16_t len; uint8_t first_char; };
static VocabSlot vocab_table[HASH_SIZE];
static TermInfo term_info[HASH_SIZE];
static uint32_t num_terms = 0;

static inline uint32_t find_or_insert_slot(const char* s, uint32_t len, uint64_t h, const char* q_buf, bool insert) {
    uint32_t h32 = (uint32_t)(h ^ (h >> 32)); if (!h32) h32 = 1;
    uint32_t idx = (uint32_t)h & HASH_MASK;
    while (vocab_table[idx].hash32) {
        if (vocab_table[idx].hash32 == h32 && term_info[vocab_table[idx].term_id].len == len &&
            memcmp(q_buf + term_info[vocab_table[idx].term_id].str_off, s, len) == 0)
            return vocab_table[idx].term_id;
        idx = (idx + 1) & HASH_MASK;
    }
    if (!insert) return UINT32_MAX;
    uint32_t tid = num_terms++;
    vocab_table[idx] = {h32, tid};
    term_info[tid] = {(uint32_t)(s - q_buf), (uint16_t)len, (uint8_t)s[0]};
    return tid;
}
static inline uint32_t vocab_lookup(const char* s, uint32_t len, uint64_t h, const char* q_buf) {
    return find_or_insert_slot(s, len, h, q_buf, false);
}
static inline uint32_t get_or_insert_term(const char* s, uint32_t len, const char* q_buf) {
    uint64_t h = 14695981039346656037ULL;
    for (uint32_t i = 0; i < len; ++i) h = (h ^ (uint8_t)s[i]) * 1099511628211ULL;
    return find_or_insert_slot(s, len, h, q_buf, true);
}

struct Query { uint8_t kind, num_terms; uint16_t ops_packed; uint32_t term_ids[8]; };
static std::vector<Query> queries;
static std::vector<uint32_t> repeat_term_map;

static uint32_t* doc_text_start = nullptr;
static uint32_t total_docs = 0, doc_start[NUM_CHUNKS], doc_end[NUM_CHUNKS];
static size_t chunk_byte_start[NUM_CHUNKS + 1];
static uint32_t num_threads = 1, words_per_doc = 0, num_bitsets = 0;

static std::vector<uint32_t> term_df, term_bitset_idx, chunk_term_offsets[NUM_CHUNKS];
static std::vector<bool> is_bitset_term;
static uint32_t chunk_base[NUM_CHUNKS];
static uint64_t* global_bitsets = nullptr;
static uint16_t* global_postings_u16 = nullptr;

struct ResultSlice { uint32_t offset, len; };
static std::vector<ResultSlice> query_results;
static char* results_buffer = nullptr;
static std::atomic<uint32_t> results_buf_ptr{0};

template <typename F>
static void run_parallel(uint32_t n, uint32_t batch, F&& fn) {
    std::atomic<uint32_t> idx{0};
    std::vector<std::thread> th;
    for (uint32_t t = 0; t < num_threads; ++t)
        th.emplace_back([&]() {
            while (true) {
                uint32_t i = idx.fetch_add(batch, std::memory_order_relaxed);
                if (i >= n) break;
                for (uint32_t j = i; j < std::min(n, i + batch); ++j) fn(j);
            }
        });
    for (auto& w : th) w.join();
}

template <typename F>
static inline void scan_doc_tokens(const char*& p, const char* q_buf, F&& fn) {
    uint32_t pos = 0, prev_tid = UINT32_MAX, prev_pos = UINT32_MAX;
    while (*p != '\n') {
        while (char_map_sep[(uint8_t)*p]) p++;
        if (*p == '\n') break;
        char tok[32]; uint32_t len = 0; uint64_t h = 14695981039346656037ULL;
        while (true) {
            uint8_t m = char_map[(uint8_t)*p]; if (!m) break;
            tok[len++] = (char)m; h = (h ^ m) * 1099511628211ULL; p++;
        }
        uint32_t tid = vocab_lookup(tok, len, h, q_buf);
        if (tid != UINT32_MAX) {
            fn(tid, (prev_tid == tid && pos == prev_pos + 1));
            prev_tid = tid; prev_pos = pos;
        } else prev_tid = UINT32_MAX;
        pos++;
    }
    if (*p == '\n') p++;
}

static void index_chunk_pass1(uint32_t c, const char* d_buf, const char* q_buf, uint32_t* counts) {
    if (doc_start[c] >= doc_end[c]) return;
    std::vector<uint32_t> last_seen(num_terms, UINT32_MAX);
    const char* p = d_buf + chunk_byte_start[c], *p_end = d_buf + chunk_byte_start[c + 1];
    while (p < p_end) {
        uint32_t d = 0;
        while (*p >= '0' && *p <= '9') d = d * 10 + (*p++ - '0');
        if (*p == '\t') p++;
        doc_text_start[d] = (uint32_t)(p - d_buf);
        scan_doc_tokens(p, q_buf, [&](uint32_t tid, bool is_repeat) {
            if (last_seen[tid] != d) { counts[tid]++; last_seen[tid] = d; }
            if (is_repeat) {
                uint32_t r_tid = (tid < repeat_term_map.size()) ? repeat_term_map[tid] : UINT32_MAX;
                if (r_tid != UINT32_MAX && last_seen[r_tid] != d) { counts[r_tid]++; last_seen[r_tid] = d; }
            }
        });
    }
}

static void index_chunk_pass2(uint32_t c, const char* d_buf, const char* q_buf) {
    uint32_t d_first = doc_start[c];
    if (d_first >= doc_end[c]) return;
    std::vector<uint32_t> last_seen(num_terms, UINT32_MAX);
    const char* p = d_buf + chunk_byte_start[c], *p_end = d_buf + chunk_byte_start[c + 1];
    auto emit = [&](uint32_t tid, uint32_t d) {
        if (is_bitset_term[tid]) {
            uint32_t b_idx = term_bitset_idx[tid];
            __atomic_fetch_or(&global_bitsets[b_idx * words_per_doc + (d >> 6)], 1ULL << (d & 63), __ATOMIC_RELAXED);
        } else {
            global_postings_u16[chunk_term_offsets[c][tid]++] = (uint16_t)(d - d_first);
        }
    };
    while (p < p_end) {
        uint32_t d = 0;
        while (*p >= '0' && *p <= '9') d = d * 10 + (*p++ - '0');
        if (*p == '\t') p++;
        scan_doc_tokens(p, q_buf, [&](uint32_t tid, bool is_repeat) {
            if (last_seen[tid] != d) { last_seen[tid] = d; emit(tid, d); }
            if (is_repeat) {
                uint32_t r_tid = (tid < repeat_term_map.size()) ? repeat_term_map[tid] : UINT32_MAX;
                if (r_tid != UINT32_MAX && last_seen[r_tid] != d) { last_seen[r_tid] = d; emit(r_tid, d); }
            }
        });
    }
}

static inline bool doc_has_term(uint32_t tid, uint32_t d, uint32_t c) {
    if (tid == UINT32_MAX) return false;
    if (is_bitset_term[tid])
        return (global_bitsets[term_bitset_idx[tid] * words_per_doc + (d >> 6)] >> (d & 63)) & 1ULL;
    uint32_t s = (tid == 0) ? chunk_base[c] : chunk_term_offsets[c][tid - 1];
    return std::binary_search(global_postings_u16 + s, global_postings_u16 + chunk_term_offsets[c][tid], (uint16_t)(d - doc_start[c]));
}

static inline void get_term_chunk_bitset(uint32_t tid, uint32_t c, uint32_t c_words, uint64_t* out) {
    if (tid == UINT32_MAX) { memset(out, 0, c_words * 8); return; }
    if (is_bitset_term[tid]) {
        memcpy(out, &global_bitsets[term_bitset_idx[tid] * words_per_doc + (doc_start[c] >> 6)], c_words * 8);
    } else {
        memset(out, 0, c_words * 8);
        uint32_t s = (tid == 0) ? chunk_base[c] : chunk_term_offsets[c][tid - 1];
        for (uint32_t i = s; i < chunk_term_offsets[c][tid]; ++i)
            out[global_postings_u16[i] >> 6] |= (1ULL << (global_postings_u16[i] & 63));
    }
}

static inline void format_result(uint64_t count, uint64_t sum, const uint32_t* top10, uint32_t top_cnt, ResultSlice& slice) {
    char buf[512], *p = buf;
    if (!count) { *p++ = '0'; *p++ = ' '; *p++ = '0'; *p++ = '\n'; }
    else {
        p = std::to_chars(p, buf + 400, count).ptr; *p++ = ' ';
        p = std::to_chars(p, buf + 400, sum).ptr;
        for (uint32_t i = 0; i < top_cnt; ++i) { *p++ = ' '; p = std::to_chars(p, buf + 400, top10[i]).ptr; }
        *p++ = '\n';
    }
    uint32_t len = (uint32_t)(p - buf);
    uint32_t offset = results_buf_ptr.fetch_add(len, std::memory_order_relaxed);
    memcpy(results_buffer + offset, buf, len);
    slice = {offset, len};
}

static void execute_bool_query(const Query& q, ResultSlice& slice) {
    uint64_t total_count = 0, total_sum = 0;
    uint32_t top10[10], top10_count = 0;

    auto accum_bits = [&](const uint64_t* b, uint32_t c_start, uint32_t c_words) {
        for (uint32_t w = 0; w < c_words; ++w) {
            uint64_t val = b[w]; if (!val) continue;
            total_count += __builtin_popcountll(val);
            while (val) {
                int bit = __builtin_ctzll(val);
                uint32_t doc_id = c_start + (w << 6) + bit;
                total_sum += doc_id;
                if (top10_count < 10) top10[top10_count++] = doc_id;
                val &= val - 1;
            }
        }
    };

    for (uint32_t c = 0; c < NUM_CHUNKS; ++c) {
        uint32_t c_start = doc_start[c], c_docs = doc_end[c] - c_start;
        if (!c_docs) continue;
        uint32_t c_words = (c_docs + 63) / 64, tid0 = q.term_ids[0];

        uint16_t cur_list[1024]; uint32_t cur_len = 0;
        bool in_bitset = is_bitset_term[tid0];
        uint64_t curr[MAX_CHUNK_WORDS];

        if (in_bitset) {
            get_term_chunk_bitset(tid0, c, c_words, curr);
        } else {
            uint32_t s0 = (tid0 == 0) ? chunk_base[c] : chunk_term_offsets[c][tid0 - 1];
            cur_len = chunk_term_offsets[c][tid0] - s0;
            if (cur_len > 1024) {
                in_bitset = true;
                get_term_chunk_bitset(tid0, c, c_words, curr);
            } else {
                memcpy(cur_list, global_postings_u16 + s0, cur_len * sizeof(uint16_t));
            }
        }

        for (uint8_t op_i = 0; op_i < q.num_terms - 1; ++op_i) {
            uint8_t op = (q.ops_packed >> (op_i * 2)) & 3;
            uint32_t n_tid = q.term_ids[op_i + 1];
            if (in_bitset) {
                uint64_t nb[MAX_CHUNK_WORDS];
                get_term_chunk_bitset(n_tid, c, c_words, nb);
                if (op == 1) for (uint32_t w = 0; w < c_words; ++w) curr[w] &= nb[w];
                else if (op == 2) for (uint32_t w = 0; w < c_words; ++w) curr[w] |= nb[w];
                else for (uint32_t w = 0; w < c_words; ++w) curr[w] &= ~nb[w];
            } else if (op == 1) { // AND
                if (!cur_len) continue;
                if (is_bitset_term[n_tid]) {
                    const uint64_t* b = &global_bitsets[term_bitset_idx[n_tid] * words_per_doc];
                    uint32_t w = 0;
                    for (uint32_t i = 0; i < cur_len; ++i) {
                        uint32_t d = c_start + cur_list[i];
                        if ((b[d >> 6] >> (d & 63)) & 1ULL) cur_list[w++] = cur_list[i];
                    }
                    cur_len = w;
                } else {
                    uint32_t ns = (n_tid == 0) ? chunk_base[c] : chunk_term_offsets[c][n_tid - 1];
                    uint32_t ne = chunk_term_offsets[c][n_tid], i = 0, j = ns, w = 0;
                    while (i < cur_len && j < ne) {
                        uint16_t a = cur_list[i], b = global_postings_u16[j];
                        if (a == b) { cur_list[w++] = a; i++; j++; }
                        else if (a < b) i++; else j++;
                    }
                    cur_len = w;
                }
            } else if (op == 3) { // NOT
                if (!cur_len) continue;
                if (is_bitset_term[n_tid]) {
                    const uint64_t* b = &global_bitsets[term_bitset_idx[n_tid] * words_per_doc];
                    uint32_t w = 0;
                    for (uint32_t i = 0; i < cur_len; ++i) {
                        uint32_t d = c_start + cur_list[i];
                        if (!((b[d >> 6] >> (d & 63)) & 1ULL)) cur_list[w++] = cur_list[i];
                    }
                    cur_len = w;
                } else {
                    uint32_t ns = (n_tid == 0) ? chunk_base[c] : chunk_term_offsets[c][n_tid - 1];
                    uint32_t ne = chunk_term_offsets[c][n_tid], i = 0, j = ns, w = 0;
                    uint16_t temp[1024];
                    while (i < cur_len && j < ne) {
                        uint16_t a = cur_list[i], b = global_postings_u16[j];
                        if (a == b) { i++; j++; }
                        else if (a < b) { temp[w++] = a; i++; } else j++;
                    }
                    while (i < cur_len) temp[w++] = cur_list[i++];
                    memcpy(cur_list, temp, w * sizeof(uint16_t));
                    cur_len = w;
                }
            } else { // OR
                uint32_t ns = (n_tid == 0) ? chunk_base[c] : chunk_term_offsets[c][n_tid - 1];
                uint32_t ne = chunk_term_offsets[c][n_tid];
                uint32_t n_len = is_bitset_term[n_tid] ? 100000 : (ne - ns);
                if (is_bitset_term[n_tid] || cur_len + n_len > 1024) {
                    memset(curr, 0, c_words * sizeof(uint64_t));
                    for (uint32_t i = 0; i < cur_len; ++i) curr[cur_list[i] >> 6] |= (1ULL << (cur_list[i] & 63));
                    in_bitset = true;
                    uint64_t nb[MAX_CHUNK_WORDS];
                    get_term_chunk_bitset(n_tid, c, c_words, nb);
                    for (uint32_t w = 0; w < c_words; ++w) curr[w] |= nb[w];
                } else {
                    uint16_t temp[1024]; uint32_t i = 0, j = ns, w = 0;
                    while (i < cur_len && j < ne) {
                        uint16_t a = cur_list[i], b = global_postings_u16[j];
                        if (a == b) { temp[w++] = a; i++; j++; }
                        else if (a < b) { temp[w++] = a; i++; } else { temp[w++] = b; j++; }
                    }
                    while (i < cur_len) temp[w++] = cur_list[i++];
                    while (j < ne) temp[w++] = global_postings_u16[j++];
                    memcpy(cur_list, temp, w * sizeof(uint16_t));
                    cur_len = w;
                }
            }
        }
        if (!in_bitset) {
            total_count += cur_len;
            for (uint32_t i = 0; i < cur_len; ++i) {
                uint32_t d = c_start + cur_list[i]; total_sum += d;
                if (top10_count < 10) top10[top10_count++] = d;
            }
        } else {
            if (c_docs & 63) curr[c_words - 1] &= (1ULL << (c_docs & 63)) - 1ULL;
            accum_bits(curr, c_start, c_words);
        }
    }
    format_result(total_count, total_sum, top10, top10_count, slice);
}

static void execute_phrase_query(const Query& q, const char* d_buf, const char* q_buf, ResultSlice& slice) {
    uint8_t k = q.num_terms;
    for (uint8_t i = 0; i < k; ++i) {
        if (q.term_ids[i] == UINT32_MAX || !term_df[q.term_ids[i]]) {
            format_result(0, 0, nullptr, 0, slice); return;
        }
    }
    uint32_t distinct[16]; uint8_t num_distinct = 0;
    for (uint8_t i = 0; i < k; ++i) {
        uint32_t tid = q.term_ids[i]; bool found = false;
        for (uint8_t j = 0; j < num_distinct; ++j) if (distinct[j] == tid) { found = true; break; }
        if (!found) distinct[num_distinct++] = tid;
    }
    for (uint8_t i = 0; i < k - 1; ++i) {
        if (q.term_ids[i] == q.term_ids[i + 1]) {
            uint32_t tid = q.term_ids[i];
            uint32_t r_tid = (tid < repeat_term_map.size()) ? repeat_term_map[tid] : UINT32_MAX;
            if (r_tid != UINT32_MAX) {
                if (!term_df[r_tid]) { format_result(0, 0, nullptr, 0, slice); return; }
                bool found = false;
                for (uint8_t j = 0; j < num_distinct; ++j) if (distinct[j] == r_tid) { found = true; break; }
                if (!found) distinct[num_distinct++] = r_tid;
            }
        }
    }
    std::sort(distinct, distinct + num_distinct, [](uint32_t a, uint32_t b) { return term_df[a] < term_df[b]; });
    if (num_distinct > 2) {
        std::stable_sort(distinct + 1, distinct + num_distinct, [](uint32_t a, uint32_t b) {
            return is_bitset_term[a] > is_bitset_term[b];
        });
    }

    struct PhraseTerm { const char* str; uint32_t len; uint8_t first_char; };
    PhraseTerm pterms[8];
    for (uint8_t i = 0; i < k; ++i) {
        uint32_t tid = q.term_ids[i];
        pterms[i] = {q_buf + term_info[tid].str_off, term_info[tid].len, term_info[tid].first_char};
    }
    struct DocToken { const char* ptr; uint32_t len; };
    uint64_t total_count = 0, total_sum = 0;
    uint32_t top10[10], top10_count = 0;

    for (uint32_t c = 0; c < NUM_CHUNKS; ++c) {
        uint32_t c_start = doc_start[c], c_docs = doc_end[c] - c_start;
        if (!c_docs) continue;
        std::vector<uint32_t> candidates;
        uint32_t rare = distinct[0];
        if (is_bitset_term[rare]) {
            uint32_t b_idx = term_bitset_idx[rare];
            const uint64_t* src = &global_bitsets[b_idx * words_per_doc + (c_start >> 6)];
            uint32_t c_words = (c_docs + 63) / 64;
            for (uint32_t w = 0; w < c_words; ++w) {
                uint64_t val = src[w];
                while (val) {
                    int bit = __builtin_ctzll(val);
                    uint32_t doc_id = c_start + (w << 6) + bit;
                    if (doc_id < doc_end[c]) candidates.push_back(doc_id);
                    val &= val - 1;
                }
            }
        } else {
            uint32_t s = (rare == 0) ? chunk_base[c] : chunk_term_offsets[c][rare - 1];
            for (uint32_t idx = s; idx < chunk_term_offsets[c][rare]; ++idx)
                candidates.push_back(c_start + global_postings_u16[idx]);
        }
        for (uint8_t term_i = 1; term_i < num_distinct && !candidates.empty(); ++term_i) {
            uint32_t other = distinct[term_i], w_idx = 0;
            for (uint32_t d : candidates) if (doc_has_term(other, d, c)) candidates[w_idx++] = d;
            candidates.resize(w_idx);
        }
        for (uint32_t d : candidates) {
            const char* p = d_buf + doc_text_start[d];
            DocToken doc_toks[1024]; uint32_t n_tok = 0;
            while (*p != '\n' && n_tok < 1024) {
                while (char_map_sep[(uint8_t)*p]) p++;
                if (*p == '\n') break;
                const char* tok_start = p;
                while (char_map[(uint8_t)*p]) p++;
                doc_toks[n_tok++] = {tok_start, (uint32_t)(p - tok_start)};
            }
            bool match = false;
            if (n_tok >= k) {
                for (uint32_t i = 0; i + k <= n_tok; ++i) {
                    if (doc_toks[i].len != pterms[0].len || char_map[(uint8_t)*doc_toks[i].ptr] != pterms[0].first_char) continue;
                    bool ok = true;
                    for (uint8_t j = 0; j < k; ++j) {
                        uint32_t tlen = pterms[j].len;
                        if (doc_toks[i + j].len != tlen) { ok = false; break; }
                        const char* d_ptr = doc_toks[i + j].ptr, *q_ptr = pterms[j].str;
                        for (uint32_t b = 0; b < tlen; ++b) {
                            if (char_map[(uint8_t)d_ptr[b]] != (uint8_t)q_ptr[b]) { ok = false; break; }
                        }
                        if (!ok) break;
                    }
                    if (ok) { match = true; break; }
                }
            }
            if (match) {
                total_count++; total_sum += d;
                if (top10_count < 10) top10[top10_count++] = d;
            }
        }
    }
    format_result(total_count, total_sum, top10, top10_count, slice);
}

static const char* mmap_file(const char* path, size_t& size) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) return nullptr;
    struct stat st; fstat(fd, &st);
    size = st.st_size;
    return (const char*)mmap(nullptr, size, PROT_READ, MAP_SHARED, fd, 0);
}

int main(int argc, char** argv) {
    init_char_map();
    const char* data_dir = "/data", *out_dir = "/out";
    for (int i = 1; i < argc; ++i) {
        if (strcmp(argv[i], "--data") == 0 && i + 1 < argc) data_dir = argv[++i];
        else if (strcmp(argv[i], "--out") == 0 && i + 1 < argc) out_dir = argv[++i];
    }
    char docs_path[512], queries_path[512], results_path[512];
    snprintf(docs_path, sizeof(docs_path), "%s/docs.txt", data_dir);
    snprintf(queries_path, sizeof(queries_path), "%s/queries.txt", data_dir);
    snprintf(results_path, sizeof(results_path), "%s/results.txt", out_dir);

    size_t q_size = 0, file_size = 0;
    const char* q_buf = mmap_file(queries_path, q_size);
    if (!q_buf) { perror("open queries"); return 1; }

    const char* q_ptr = q_buf, *q_end = q_buf + q_size;
    while (q_ptr < q_end) {
        while (q_ptr < q_end && (*q_ptr == ' ' || *q_ptr == '\r' || *q_ptr == '\n')) q_ptr++;
        if (q_ptr >= q_end) break;
        bool is_bool = (strncmp(q_ptr, "BOOL ", 5) == 0);
        q_ptr += is_bool ? 5 : 8;
        const char* line_end = is_bool ? (const char*)memchr(q_ptr, '\n', q_end - q_ptr) : (const char*)memchr(q_ptr, '\"', q_end - q_ptr);
        if (!line_end) line_end = q_end;
        Query q = {(uint8_t)(is_bool ? 0 : 1), 0, 0, {0}};
        while (q_ptr < line_end) {
            while (q_ptr < line_end && *q_ptr == ' ') q_ptr++;
            if (q_ptr >= line_end) break;
            const char* tok = q_ptr;
            while (q_ptr < line_end && *q_ptr != ' ') q_ptr++;
            uint32_t len = (uint32_t)(q_ptr - tok);
            if (is_bool && len == 3 && memcmp(tok, "AND", 3) == 0) q.ops_packed |= (1 << ((q.num_terms - 1) * 2));
            else if (is_bool && len == 2 && memcmp(tok, "OR", 2) == 0) q.ops_packed |= (2 << ((q.num_terms - 1) * 2));
            else if (is_bool && len == 3 && memcmp(tok, "NOT", 3) == 0) q.ops_packed |= (3 << ((q.num_terms - 1) * 2));
            else q.term_ids[q.num_terms++] = get_or_insert_term(tok, len, q_buf);
        }
        queries.push_back(q);
        q_ptr = (const char*)memchr(line_end, '\n', q_end - line_end);
        if (q_ptr) q_ptr++; else break;
    }

    repeat_term_map.assign(num_terms, UINT32_MAX);
    for (const auto& q : queries) {
        if (q.kind == 1) {
            for (uint8_t i = 0; i < q.num_terms - 1; ++i) {
                if (q.term_ids[i] == q.term_ids[i + 1]) {
                    uint32_t tid = q.term_ids[i];
                    if (repeat_term_map[tid] == UINT32_MAX) {
                        uint32_t r_tid = num_terms++;
                        repeat_term_map[tid] = r_tid;
                        term_info[r_tid] = term_info[tid];
                    }
                }
            }
        }
    }

    const char* d_buf = mmap_file(docs_path, file_size);
    if (!d_buf) { perror("open docs"); return 1; }

    if (file_size > 0) {
        const char* p_last = d_buf + file_size - 1;
        while (p_last > d_buf && *(p_last - 1) != '\n') p_last--;
        uint32_t last_id = 0;
        while (p_last < d_buf + file_size && *p_last >= '0' && *p_last <= '9')
            last_id = last_id * 10 + (*p_last++ - '0');
        total_docs = last_id + 1;
    }

    doc_text_start = (uint32_t*)malloc(total_docs * sizeof(uint32_t));

    uint32_t docs_per_chunk = (total_docs + NUM_CHUNKS - 1) / NUM_CHUNKS;
    for (uint32_t c = 0; c < NUM_CHUNKS; ++c) {
        uint32_t start = c * docs_per_chunk;
        if (c > 0) start = (start + 63) & ~63u;
        if (start > total_docs) start = total_docs;
        doc_start[c] = start;
        if (c > 0) doc_end[c - 1] = start;
    }
    doc_end[NUM_CHUNKS - 1] = total_docs;

    // Safety guard: ensure local chunk doc IDs fit within uint16_t (max 65535)
    for (uint32_t c = 0; c < NUM_CHUNKS; ++c) {
        if (__builtin_expect(doc_end[c] - doc_start[c] > 65535, 0)) abort();
    }

    chunk_byte_start[0] = 0;
    chunk_byte_start[NUM_CHUNKS] = file_size;
    for (uint32_t c = 1; c < NUM_CHUNKS; ++c) {
        uint32_t target_id = doc_start[c];
        if (target_id >= total_docs) { chunk_byte_start[c] = file_size; continue; }
        int64_t low = 0, high = (int64_t)file_size - 1, ans = (int64_t)file_size;
        while (low <= high) {
            int64_t mid = low + (high - low) / 2, p = mid;
            while (p < (int64_t)file_size && p > 0 && d_buf[p - 1] != '\n') p++;
            if (p >= (int64_t)file_size) { high = mid - 1; continue; }
            uint32_t cur_id = 0; const char* cp = d_buf + p;
            while (cp < d_buf + file_size && *cp >= '0' && *cp <= '9') cur_id = cur_id * 10 + (*cp++ - '0');
            if (cur_id >= target_id) { ans = p; high = mid - 1; }
            else low = p + 1;
        }
        chunk_byte_start[c] = (size_t)ans;
    }

    num_threads = std::min(NUM_CHUNKS, std::thread::hardware_concurrency());
    if (!num_threads) num_threads = 1;

    size_t chunk_counts_bytes = (size_t)NUM_CHUNKS * num_terms * sizeof(uint32_t);
    uint32_t* chunk_counts = (uint32_t*)mmap(nullptr, chunk_counts_bytes, PROT_READ | PROT_WRITE,
                                             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);

    run_parallel(NUM_CHUNKS, 1, [&](uint32_t c) {
        index_chunk_pass1(c, d_buf, q_buf, chunk_counts + c * num_terms);
    });

    term_df.assign(num_terms, 0);
    for (uint32_t i = 0; i < num_terms; ++i)
        for (uint32_t c = 0; c < NUM_CHUNKS; ++c) term_df[i] += chunk_counts[c * num_terms + i];

    is_bitset_term.assign(num_terms, false);
    term_bitset_idx.assign(num_terms, UINT32_MAX);
    for (uint32_t i = 0; i < num_terms; ++i) {
        if (term_df[i] > BITSET_THRESHOLD) {
            is_bitset_term[i] = true;
            term_bitset_idx[i] = num_bitsets++;
        }
    }

    words_per_doc = (total_docs + 63) / 64;
    global_bitsets = (uint64_t*)calloc((size_t)num_bitsets * words_per_doc, sizeof(uint64_t));

    uint32_t total_u16_postings = 0;
    for (uint32_t c = 0; c < NUM_CHUNKS; ++c) {
        chunk_base[c] = total_u16_postings;
        chunk_term_offsets[c].resize(num_terms);
        for (uint32_t i = 0; i < num_terms; ++i) {
            chunk_term_offsets[c][i] = total_u16_postings;
            if (!is_bitset_term[i]) total_u16_postings += chunk_counts[c * num_terms + i];
        }
    }
    global_postings_u16 = (uint16_t*)malloc((size_t)total_u16_postings * sizeof(uint16_t));
    munmap(chunk_counts, chunk_counts_bytes);

    run_parallel(NUM_CHUNKS, 1, [&](uint32_t c) {
        index_chunk_pass2(c, d_buf, q_buf);
    });

    uint32_t num_q = (uint32_t)queries.size();
    query_results.resize(num_q);
    results_buffer = (char*)malloc(num_q * 128);

    run_parallel(num_q, 64, [&](uint32_t idx) {
        const auto& q = queries[idx];
        if (q.kind == 0) execute_bool_query(q, query_results[idx]);
        else execute_phrase_query(q, d_buf, q_buf, query_results[idx]);
    });

    int fd_out = open(results_path, O_WRONLY | O_CREAT | O_TRUNC, 0666);
    if (fd_out < 0) { perror("open out"); return 1; }

    char write_buf[1 << 16]; uint32_t write_pos = 0;
    for (uint32_t i = 0; i < num_q; ++i) {
        const auto& sl = query_results[i];
        if (write_pos + sl.len > sizeof(write_buf)) {
            ssize_t ret = write(fd_out, write_buf, write_pos); (void)ret;
            write_pos = 0;
        }
        memcpy(write_buf + write_pos, results_buffer + sl.offset, sl.len);
        write_pos += sl.len;
    }
    if (write_pos > 0) {
        ssize_t ret = write(fd_out, write_buf, write_pos); (void)ret;
    }
    close(fd_out);
    return 0;
}
