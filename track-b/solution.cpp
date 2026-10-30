/**
 * Speed Demon 2026 - Track B (Index)
 * Single-Pass SWAR + AVX2 + Bigram-Accelerated CSR/Bitset Engine
 */
#pragma GCC optimize("O3,unroll-loops,fast-math")
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

static constexpr uint32_t HASH_SIZE = 131072, HASH_MASK = HASH_SIZE - 1;
static constexpr uint32_t BIGRAM_HASH_SIZE = 65536, BIGRAM_MASK = BIGRAM_HASH_SIZE - 1;
static constexpr uint32_t BITSET_THRESHOLD = 14000, NUM_CHUNKS = 12, MAX_CHUNK_WORDS = 512;

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
struct TermInfo {
    uint64_t w0, w1;
    uint32_t str_off;
    uint16_t len;
    uint8_t first_char;
};
struct BigramSlot { uint64_t key; uint32_t b_tid; };

static VocabSlot vocab_table[HASH_SIZE];
static BigramSlot bigram_table[BIGRAM_HASH_SIZE];
static TermInfo term_info[131072];
alignas(64) static uint64_t bloom[8192];
alignas(64) static uint64_t first_term_bits[2048];
static uint32_t num_base_terms = 0, num_terms = 0;

static inline uint64_t mix64(uint64_t w0, uint64_t w1, uint32_t len) {
    uint64_t h = (w0 ^ ((w1 * 0x9e3779b97f4a7c15ULL) + len)) * 0xbf58476d1ce4e5b9ULL;
    return h ^ (h >> 31);
}

static inline uint32_t vocab_lookup_swar(uint64_t w0, uint64_t w1, const char* s_tail, uint32_t len, const char* q_buf, uint64_t h) {
    uint32_t h32 = (uint32_t)(h ^ (h >> 32)); if (!h32) h32 = 1;
    uint32_t idx = (uint32_t)h & HASH_MASK;
    while (vocab_table[idx].hash32) {
        if (vocab_table[idx].hash32 == h32) {
            const auto& ti = term_info[vocab_table[idx].term_id];
            if (ti.w0 == w0 && ti.w1 == w1 && ti.len == len &&
                (__builtin_expect(len <= 16, 1) || memcmp(q_buf + ti.str_off + 16, s_tail, len - 16) == 0))
                return vocab_table[idx].term_id;
        }
        idx = (idx + 1) & HASH_MASK;
    }
    return UINT32_MAX;
}

static inline uint32_t get_or_insert_term(const char* s, uint32_t len, const char* q_buf) {
    uint64_t w0 = 0, w1 = 0;
    memcpy(&w0, s, std::min(len, 8u));
    if (len > 8) memcpy(&w1, s + 8, std::min(len - 8u, 8u));
    uint64_t h = mix64(w0, w1, len);
    uint32_t h32 = (uint32_t)(h ^ (h >> 32)); if (!h32) h32 = 1;
    uint32_t idx = (uint32_t)h & HASH_MASK;
    while (vocab_table[idx].hash32) {
        if (vocab_table[idx].hash32 == h32) {
            const auto& ti = term_info[vocab_table[idx].term_id];
            if (ti.w0 == w0 && ti.w1 == w1 && ti.len == len &&
                (len <= 16 || memcmp(q_buf + ti.str_off + 16, s + 16, len - 16) == 0))
                return vocab_table[idx].term_id;
        }
        idx = (idx + 1) & HASH_MASK;
    }
    uint32_t tid = num_terms++;
    vocab_table[idx] = {h32, tid};
    term_info[tid] = {w0, w1, (uint32_t)(s - q_buf), (uint16_t)len, (uint8_t)s[0]};
    bloom[(h >> 32) & 8191] |= (1ULL << (h & 63));
    return tid;
}

static inline uint32_t get_or_insert_bigram(uint32_t t1, uint32_t t2) {
    uint64_t key = ((uint64_t)t1 << 32) | t2 | (1ULL << 63);
    uint64_t h = (key * 0x9e3779b97f4a7c15ULL);
    uint32_t idx = (uint32_t)(h ^ (h >> 32)) & BIGRAM_MASK;
    while (bigram_table[idx].key) {
        if (bigram_table[idx].key == key) return bigram_table[idx].b_tid;
        idx = (idx + 1) & BIGRAM_MASK;
    }
    uint32_t b_tid = num_terms++;
    bigram_table[idx] = {key, b_tid};
    first_term_bits[t1 >> 6] |= (1ULL << (t1 & 63));
    return b_tid;
}

static inline uint32_t lookup_bigram(uint32_t t1, uint32_t t2) {
    uint64_t key = ((uint64_t)t1 << 32) | t2 | (1ULL << 63);
    uint64_t h = (key * 0x9e3779b97f4a7c15ULL);
    uint32_t idx = (uint32_t)(h ^ (h >> 32)) & BIGRAM_MASK;
    while (bigram_table[idx].key) {
        if (bigram_table[idx].key == key) return bigram_table[idx].b_tid;
        idx = (idx + 1) & BIGRAM_MASK;
    }
    return UINT32_MAX;
}

struct Query { uint8_t kind, num_terms; uint16_t ops_packed; uint32_t term_ids[8], bigram_ids[7]; };
static std::vector<Query> queries;

static uint32_t* doc_text_start = nullptr;
static uint32_t total_docs = 0, doc_start[NUM_CHUNKS], doc_end[NUM_CHUNKS];
static size_t chunk_byte_start[NUM_CHUNKS + 1];
static uint32_t num_threads = 1, words_per_doc = 0, num_bitsets = 0;
static bool has_long_phrases = false;

static std::vector<uint32_t> term_df, term_bitset_idx, chunk_term_offsets[NUM_CHUNKS];
static std::vector<uint8_t> is_bitset_term;
static uint32_t chunk_base[NUM_CHUNKS];
static uint64_t* global_bitsets = nullptr;
static uint16_t* global_postings_u16 = nullptr;

struct ChunkTemp {
    uint16_t* post_doc = nullptr;
    uint32_t* post_tid = nullptr;
    uint32_t count = 0, cap = 0;
};
static ChunkTemp chunk_temps[NUM_CHUNKS];

struct ResultSlice { uint32_t offset, len; };
static std::vector<ResultSlice> query_results;
static char* results_buffer = nullptr;
static std::atomic<uint32_t> results_buf_ptr{0};

template <typename F>
static void run_parallel(uint32_t n, uint32_t batch, F&& fn) {
    std::atomic<uint32_t> idx{0};
    std::vector<std::thread> th;
    th.reserve(num_threads);
    for (uint32_t t = 0; t < num_threads; ++t)
        th.emplace_back([&]() {
            while (true) {
                uint32_t i = idx.fetch_add(batch, std::memory_order_relaxed);
                if (i >= n) break;
                uint32_t end = std::min(n, i + batch);
                for (uint32_t j = i; j < end; ++j) fn(j);
            }
        });
    for (auto& w : th) w.join();
}

static void index_chunk_single_pass(uint32_t c, const char* d_buf, const char* q_buf, uint32_t* counts, uint32_t* last_seen) {
    uint32_t d_first = doc_start[c];
    if (d_first >= doc_end[c]) return;
    memset(last_seen, 0xFF, num_terms * sizeof(uint32_t));
    const char* p = d_buf + chunk_byte_start[c], *p_end = d_buf + chunk_byte_start[c + 1];

    uint32_t cap = 1 << 20;
    uint16_t* p_doc = (uint16_t*)malloc(cap * sizeof(uint16_t));
    uint32_t* p_tid = (uint32_t*)malloc(cap * sizeof(uint32_t));
    uint32_t cnt = 0;

    auto record = [&](uint32_t tid, uint32_t d, uint16_t local_d) {
        if (last_seen[tid] != d) {
            last_seen[tid] = d;
            counts[tid]++;
            if (__builtin_expect(cnt == cap, 0)) {
                cap <<= 1;
                p_doc = (uint16_t*)realloc(p_doc, cap * sizeof(uint16_t));
                p_tid = (uint32_t*)realloc(p_tid, cap * sizeof(uint32_t));
            }
            p_doc[cnt] = local_d;
            p_tid[cnt] = tid;
            cnt++;
        }
    };

    while (p < p_end) {
        uint32_t d = 0;
        while (*p >= '0' && *p <= '9') d = d * 10 + (*p++ - '0');
        if (*p == '\t') p++;
        if (has_long_phrases) doc_text_start[d] = (uint32_t)(p - d_buf);
        uint16_t local_d = (uint16_t)(d - d_first);
        uint32_t prev_tid = UINT32_MAX;

        while (*p != '\n') {
            while (char_map_sep[(uint8_t)*p]) p++;
            if (*p == '\n') break;
            uint64_t w0 = 0, w1 = 0;
            char tail[16];
            uint32_t len = 0;
            uint8_t m = char_map[(uint8_t)*p];
            do {
                if (len < 8) w0 |= ((uint64_t)m << (len * 8));
                else if (len < 16) w1 |= ((uint64_t)m << ((len - 8) * 8));
                else if (len < 32) tail[len - 16] = (char)m;
                len++; p++;
                m = char_map[(uint8_t)*p];
            } while (m);
            uint64_t h = mix64(w0, w1, len);
            uint32_t tid = UINT32_MAX;
            if ((bloom[(h >> 32) & 8191] >> (h & 63)) & 1ULL) {
                tid = vocab_lookup_swar(w0, w1, tail, len, q_buf, h);
            }
            if (tid != UINT32_MAX) {
                record(tid, d, local_d);
                if (prev_tid != UINT32_MAX && ((first_term_bits[prev_tid >> 6] >> (prev_tid & 63)) & 1ULL)) {
                    uint32_t b_tid = lookup_bigram(prev_tid, tid);
                    if (b_tid != UINT32_MAX) record(b_tid, d, local_d);
                }
                prev_tid = tid;
            } else {
                prev_tid = UINT32_MAX;
            }
        }
        p++;
    }
    chunk_temps[c] = {p_doc, p_tid, cnt, cap};
}

static void populate_chunk_index(uint32_t c) {
    uint32_t d_first = doc_start[c];
    if (d_first >= doc_end[c]) return;
    const auto& ct = chunk_temps[c];
    uint32_t* c_offsets = chunk_term_offsets[c].data();
    const uint8_t* is_bs = is_bitset_term.data();
    const uint32_t* bs_idx = term_bitset_idx.data();
    const uint16_t* p_doc = ct.post_doc;
    const uint32_t* p_tid = ct.post_tid;
    uint32_t cnt = ct.count;

    for (uint32_t i = 0; i < cnt; ++i) {
        uint32_t tid = p_tid[i];
        uint16_t local_d = p_doc[i];
        if (is_bs[tid]) {
            uint32_t d = d_first + local_d;
            global_bitsets[(size_t)bs_idx[tid] * words_per_doc + (d >> 6)] |= (1ULL << (d & 63));
        } else {
            global_postings_u16[c_offsets[tid]++] = local_d;
        }
    }
    free(ct.post_doc);
    free(ct.post_tid);
}

static inline void get_term_chunk_bitset(uint32_t tid, uint32_t c, uint32_t c_words, uint64_t* out) {
    if (tid == UINT32_MAX) { memset(out, 0, c_words * 8); return; }
    if (is_bitset_term[tid]) {
        memcpy(out, &global_bitsets[(size_t)term_bitset_idx[tid] * words_per_doc + (doc_start[c] >> 6)], c_words * 8);
    } else {
        memset(out, 0, c_words * 8);
        uint32_t s = (tid == 0) ? chunk_base[c] : chunk_term_offsets[c][tid - 1];
        uint32_t e = chunk_term_offsets[c][tid];
        for (uint32_t i = s; i < e; ++i)
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
            uint32_t base_id = c_start + (w << 6);
            while (val) {
                uint32_t doc_id = base_id + __builtin_ctzll(val);
                total_sum += doc_id;
                if (__builtin_expect(top10_count < 10, 0)) top10[top10_count++] = doc_id;
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
        alignas(32) uint64_t curr[MAX_CHUNK_WORDS];

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
                if (is_bitset_term[n_tid]) {
                    const uint64_t* nb = &global_bitsets[(size_t)term_bitset_idx[n_tid] * words_per_doc + (c_start >> 6)];
                    if (op == 1) for (uint32_t w = 0; w < c_words; ++w) curr[w] &= nb[w];
                    else if (op == 2) for (uint32_t w = 0; w < c_words; ++w) curr[w] |= nb[w];
                    else for (uint32_t w = 0; w < c_words; ++w) curr[w] &= ~nb[w];
                } else {
                    uint32_t ns = (n_tid == 0) ? chunk_base[c] : chunk_term_offsets[c][n_tid - 1];
                    uint32_t ne = chunk_term_offsets[c][n_tid];
                    if (op == 2) {
                        for (uint32_t i = ns; i < ne; ++i)
                            curr[global_postings_u16[i] >> 6] |= (1ULL << (global_postings_u16[i] & 63));
                    } else if (op == 3) {
                        for (uint32_t i = ns; i < ne; ++i)
                            curr[global_postings_u16[i] >> 6] &= ~(1ULL << (global_postings_u16[i] & 63));
                    } else {
                        alignas(32) uint64_t nb[MAX_CHUNK_WORDS];
                        get_term_chunk_bitset(n_tid, c, c_words, nb);
                        for (uint32_t k = 0; k < c_words; ++k) curr[k] &= nb[k];
                    }
                }
            } else if (op == 1) { // AND
                if (!cur_len) continue;
                if (is_bitset_term[n_tid]) {
                    const uint64_t* b = &global_bitsets[(size_t)term_bitset_idx[n_tid] * words_per_doc];
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
                    const uint64_t* b = &global_bitsets[(size_t)term_bitset_idx[n_tid] * words_per_doc];
                    uint32_t w = 0;
                    for (uint32_t i = 0; i < cur_len; ++i) {
                        uint32_t d = c_start + cur_list[i];
                        if (!((b[d >> 6] >> (d & 63)) & 1ULL)) cur_list[w++] = cur_list[i];
                    }
                    cur_len = w;
                } else {
                    uint32_t ns = (n_tid == 0) ? chunk_base[c] : chunk_term_offsets[c][n_tid - 1];
                    uint32_t ne = chunk_term_offsets[c][n_tid], i = 0, j = ns, w = 0;
                    while (i < cur_len && j < ne) {
                        uint16_t a = cur_list[i], b = global_postings_u16[j];
                        if (a == b) { i++; j++; }
                        else if (a < b) { cur_list[w++] = a; i++; } else j++;
                    }
                    while (i < cur_len) cur_list[w++] = cur_list[i++];
                    cur_len = w;
                }
            } else { // OR
                uint32_t ns = (n_tid == 0) ? chunk_base[c] : chunk_term_offsets[c][n_tid - 1];
                uint32_t ne = chunk_term_offsets[c][n_tid];
                uint32_t n_len = is_bitset_term[n_tid] ? 100000 : (ne - ns);
                if (is_bitset_term[n_tid] || cur_len + n_len > 1024) {
                    get_term_chunk_bitset(n_tid, c, c_words, curr);
                    for (uint32_t i = 0; i < cur_len; ++i) curr[cur_list[i] >> 6] |= (1ULL << (cur_list[i] & 63));
                    in_bitset = true;
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
                if (__builtin_expect(top10_count < 10, 0)) top10[top10_count++] = d;
            }
        } else {
            if (c_docs & 63) curr[c_words - 1] &= (1ULL << (c_docs & 63)) - 1ULL;
            accum_bits(curr, c_start, c_words);
        }
    }
    format_result(total_count, total_sum, top10, top10_count, slice);
}

static inline bool token_eq_term(const char* p, uint32_t len, const TermInfo& ti, const char* q_buf) {
    if (len != ti.len) return false;
    const char* q = q_buf + ti.str_off;
    for (uint32_t i = 0; i < len; ++i)
        if (char_map[(uint8_t)p[i]] != (uint8_t)q[i]) return false;
    return true;
}

static void execute_phrase_query(const Query& q, const char* d_buf, const char* q_buf, ResultSlice& slice) {
    uint8_t k = q.num_terms;
    for (uint8_t i = 0; i < k; ++i) {
        if (q.term_ids[i] == UINT32_MAX || !term_df[q.term_ids[i]]) {
            format_result(0, 0, nullptr, 0, slice); return;
        }
    }
    for (uint8_t i = 0; i < k - 1; ++i) {
        if (q.bigram_ids[i] == UINT32_MAX || !term_df[q.bigram_ids[i]]) {
            format_result(0, 0, nullptr, 0, slice); return;
        }
    }

    // Fast Path: 1-term or 2-term phrase query is 100% exact via single term / bigram index!
    if (k <= 2) {
        uint32_t exact_tid = (k == 1) ? q.term_ids[0] : q.bigram_ids[0];
        Query bq = {0, 1, 0, {exact_tid}, {0}};
        execute_bool_query(bq, slice);
        return;
    }

    uint32_t distinct[16]; uint8_t num_distinct = 0;
    for (uint8_t i = 0; i < k - 1; ++i) {
        uint32_t b_tid = q.bigram_ids[i]; bool found = false;
        for (uint8_t j = 0; j < num_distinct; ++j) if (distinct[j] == b_tid) { found = true; break; }
        if (!found) distinct[num_distinct++] = b_tid;
    }
    std::sort(distinct, distinct + num_distinct, [](uint32_t a, uint32_t b) { return term_df[a] < term_df[b]; });

    TermInfo pterms[8];
    for (uint8_t i = 0; i < k; ++i) pterms[i] = term_info[q.term_ids[i]];

    uint64_t total_count = 0, total_sum = 0;
    uint32_t top10[10], top10_count = 0;
    uint16_t cand_u16[32768];

    for (uint32_t c = 0; c < NUM_CHUNKS; ++c) {
        uint32_t c_start = doc_start[c], c_docs = doc_end[c] - c_start;
        if (!c_docs) continue;
        uint32_t rare = distinct[0], cand_len = 0;
        const uint16_t* cand_ptr = cand_u16;

        if (!is_bitset_term[rare]) {
            uint32_t s = (rare == 0) ? chunk_base[c] : chunk_term_offsets[c][rare - 1];
            uint32_t e = chunk_term_offsets[c][rare];
            cand_len = e - s;
            if (!cand_len) continue;
            if (num_distinct == 1) cand_ptr = global_postings_u16 + s;
            else memcpy(cand_u16, global_postings_u16 + s, cand_len * sizeof(uint16_t));
        } else {
            const uint64_t* src = &global_bitsets[(size_t)term_bitset_idx[rare] * words_per_doc + (c_start >> 6)];
            uint32_t c_words = (c_docs + 63) / 64;
            for (uint32_t w = 0; w < c_words; ++w) {
                uint64_t val = src[w];
                while (val) {
                    uint32_t local_id = (w << 6) + __builtin_ctzll(val);
                    if (local_id < c_docs) cand_u16[cand_len++] = (uint16_t)local_id;
                    val &= val - 1;
                }
            }
            if (!cand_len) continue;
        }

        for (uint8_t term_i = 1; term_i < num_distinct && cand_len; ++term_i) {
            uint32_t other = distinct[term_i], w_idx = 0;
            if (is_bitset_term[other]) {
                const uint64_t* b = &global_bitsets[(size_t)term_bitset_idx[other] * words_per_doc];
                for (uint32_t i = 0; i < cand_len; ++i) {
                    uint32_t d = c_start + cand_u16[i];
                    if ((b[d >> 6] >> (d & 63)) & 1ULL) cand_u16[w_idx++] = cand_u16[i];
                }
            } else {
                uint32_t ns = (other == 0) ? chunk_base[c] : chunk_term_offsets[c][other - 1];
                uint32_t ne = chunk_term_offsets[c][other], i = 0, j = ns;
                while (i < cand_len && j < ne) {
                    uint16_t a = cand_u16[i], b = global_postings_u16[j];
                    if (a == b) { cand_u16[w_idx++] = a; i++; j++; }
                    else if (a < b) i++; else j++;
                }
            }
            cand_len = w_idx;
        }

        const uint8_t c0_lower = pterms[0].first_char;
        const uint32_t len0 = pterms[0].len;
        for (uint32_t ci = 0; ci < cand_len; ++ci) {
            uint32_t d = c_start + cand_ptr[ci];
            const char* p = d_buf + doc_text_start[d];
            bool match = false;

            while (*p != '\n') {
                while (char_map_sep[(uint8_t)*p]) p++;
                if (*p == '\n') break;
                const char* t0_start = p;
                uint8_t first_c = char_map[(uint8_t)*p++];
                while (char_map[(uint8_t)*p]) p++;
                uint32_t t0_len = (uint32_t)(p - t0_start);

                if (t0_len == len0 && first_c == c0_lower && token_eq_term(t0_start, t0_len, pterms[0], q_buf)) {
                    const char* scan_p = p;
                    uint8_t j = 1;
                    for (; j < k; ++j) {
                        while (char_map_sep[(uint8_t)*scan_p]) scan_p++;
                        if (*scan_p == '\n') break;
                        const char* tj_start = scan_p;
                        uint8_t cj = char_map[(uint8_t)*scan_p++];
                        while (char_map[(uint8_t)*scan_p]) scan_p++;
                        uint32_t tj_len = (uint32_t)(scan_p - tj_start);
                        if (tj_len != pterms[j].len || cj != pterms[j].first_char ||
                            !token_eq_term(tj_start, tj_len, pterms[j], q_buf)) break;
                    }
                    if (j == k) { match = true; break; }
                }
            }
            if (match) {
                total_count++; total_sum += d;
                if (__builtin_expect(top10_count < 10, 0)) top10[top10_count++] = d;
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
    const char* p = (const char*)mmap(nullptr, size, PROT_READ, MAP_PRIVATE, fd, 0);
    close(fd);
    return p;
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
    if (!q_buf) return 1;

    const char* q_ptr = q_buf, *q_end = q_buf + q_size;
    while (q_ptr < q_end) {
        while (q_ptr < q_end && (*q_ptr == ' ' || *q_ptr == '\r' || *q_ptr == '\n')) q_ptr++;
        if (q_ptr >= q_end) break;
        bool is_bool = (strncmp(q_ptr, "BOOL ", 5) == 0);
        q_ptr += is_bool ? 5 : 8;
        const char* line_end = is_bool ? (const char*)memchr(q_ptr, '\n', q_end - q_ptr) : (const char*)memchr(q_ptr, '\"', q_end - q_ptr);
        if (!line_end) line_end = q_end;
        Query q = {(uint8_t)(is_bool ? 0 : 1), 0, 0, {0}, {0}};
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

    num_base_terms = num_terms;
    for (auto& q : queries) {
        if (q.kind == 1) {
            if (q.num_terms > 2) has_long_phrases = true;
            for (uint8_t i = 0; i < q.num_terms - 1; ++i) {
                q.bigram_ids[i] = get_or_insert_bigram(q.term_ids[i], q.term_ids[i + 1]);
            }
        }
    }

    const char* d_buf = mmap_file(docs_path, file_size);
    if (!d_buf) return 1;

    if (file_size > 0) {
        const char* p_last = d_buf + file_size - 1;
        while (p_last > d_buf && *(p_last - 1) != '\n') p_last--;
        uint32_t last_id = 0;
        while (p_last < d_buf + file_size && *p_last >= '0' && *p_last <= '9')
            last_id = last_id * 10 + (*p_last++ - '0');
        total_docs = last_id + 1;
    }

    if (has_long_phrases) {
        doc_text_start = (uint32_t*)malloc(total_docs * sizeof(uint32_t));
    }

    uint32_t docs_per_chunk = (total_docs + NUM_CHUNKS - 1) / NUM_CHUNKS;
    for (uint32_t c = 0; c < NUM_CHUNKS; ++c) {
        uint32_t start = c * docs_per_chunk;
        if (c > 0) start = (start + 511) & ~511u; // 512-doc (64-byte cacheline) alignment
        if (start > total_docs) start = total_docs;
        doc_start[c] = start;
        if (c > 0) doc_end[c - 1] = start;
    }
    doc_end[NUM_CHUNKS - 1] = total_docs;

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

    size_t scratch_bytes = (size_t)NUM_CHUNKS * num_terms * sizeof(uint32_t) + (size_t)num_threads * num_terms * sizeof(uint32_t);
    uint32_t* scratch = (uint32_t*)mmap(nullptr, scratch_bytes, PROT_READ | PROT_WRITE,
                                        MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    uint32_t* chunk_counts = scratch;
    uint32_t* chunk_last_seen = scratch + (size_t)NUM_CHUNKS * num_terms;

    {
        std::atomic<uint32_t> chunk_idx{0};
        std::vector<std::thread> th;
        for (uint32_t t = 0; t < num_threads; ++t) {
            th.emplace_back([&, t]() {
                uint32_t* my_last_seen = chunk_last_seen + (size_t)t * num_terms;
                while (true) {
                    uint32_t c = chunk_idx.fetch_add(1, std::memory_order_relaxed);
                    if (c >= NUM_CHUNKS) break;
                    index_chunk_single_pass(c, d_buf, q_buf, chunk_counts + c * num_terms, my_last_seen);
                }
            });
        }
        for (auto& w : th) w.join();
    }

    if (!has_long_phrases) {
        madvise((void*)d_buf, file_size, MADV_DONTNEED);
    }

    term_df.assign(num_terms, 0);
    for (uint32_t c = 0; c < NUM_CHUNKS; ++c) {
        const uint32_t* cc = chunk_counts + c * num_terms;
        for (uint32_t i = 0; i < num_terms; ++i) term_df[i] += cc[i];
    }

    is_bitset_term.assign(num_terms, 0);
    term_bitset_idx.assign(num_terms, UINT32_MAX);
    for (uint32_t i = 0; i < num_terms; ++i) {
        if (term_df[i] > BITSET_THRESHOLD) {
            is_bitset_term[i] = 1;
            term_bitset_idx[i] = num_bitsets++;
        }
    }

    words_per_doc = (total_docs + 63) / 64;
    global_bitsets = (uint64_t*)calloc((size_t)num_bitsets * words_per_doc, sizeof(uint64_t));

    uint32_t total_u16_postings = 0;
    for (uint32_t c = 0; c < NUM_CHUNKS; ++c) {
        chunk_base[c] = total_u16_postings;
        chunk_term_offsets[c].resize(num_terms);
        const uint32_t* cc = chunk_counts + c * num_terms;
        for (uint32_t i = 0; i < num_terms; ++i) {
            chunk_term_offsets[c][i] = total_u16_postings;
            if (!is_bitset_term[i]) total_u16_postings += cc[i];
        }
    }
    munmap(scratch, scratch_bytes);

    global_postings_u16 = (uint16_t*)malloc((size_t)total_u16_postings * sizeof(uint16_t));

    run_parallel(NUM_CHUNKS, 1, [&](uint32_t c) {
        populate_chunk_index(c);
    });

    uint32_t num_q = (uint32_t)queries.size();
    query_results.resize(num_q);
    results_buffer = (char*)malloc((size_t)num_q * 48 + 65536);

    run_parallel(num_q, 64, [&](uint32_t idx) {
        const auto& q = queries[idx];
        if (q.kind == 0) execute_bool_query(q, query_results[idx]);
        else execute_phrase_query(q, d_buf, q_buf, query_results[idx]);
    });

    int fd_out = open(results_path, O_WRONLY | O_CREAT | O_TRUNC, 0666);
    if (fd_out < 0) return 1;

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
