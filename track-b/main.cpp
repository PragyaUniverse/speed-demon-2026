// Track B: 50,000 boolean and phrase queries over a corpus, with no global index at all.
//
// The queries are known up front, so the corpus is cut into chunks of a few thousand documents and
// each worker handles one chunk end to end: tokenise it, keep only postings of query terms, answer
// every BOOL query on that chunk with small bitsets, and verify each phrase at the occurrences of
// its rarest term by reading the text around them. Memory is bounded by the chunk, not the corpus.
#include <fcntl.h>
#ifdef __x86_64__
#include <immintrin.h>
#endif
#include <sys/prctl.h>
#include <sys/stat.h>
#include <unistd.h>

#include <algorithm>
#include <atomic>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <string>
#include <thread>
#include <vector>

using u8 = uint8_t;
using u16 = uint16_t;
using u32 = uint32_t;
using u64 = uint64_t;

constexpr size_t CHUNK_BYTES = 1 << 21, SAMPLE_BYTES = 1 << 18, READ_BYTES = 1 << 18, PAD = 128;
constexpr u32 REL_BITS = 12;  // A 2 MB chunk holds about 2,000 documents; asserted to fit in 4,096.
constexpr u32 NONE = ~0u, FREQUENT = 8;  // Terms this common in the sample write bitsets directly.
constexpr size_t STRIDE = 1 << (REL_BITS - 6);  // Words per bitset row, enough for any chunk.
constexpr u32 TERM_BITS = 18, TABLE_BITS = 17;
constexpr u64 LOWER = 0x2020202020202020;
constexpr u32 BOOL_TERM = 1u << 31, ANCHOR_TERM = 1u << 30;

#define INLINE inline __attribute__((always_inline))

static INLINE u64 load(const u8* p) {
    u64 v;
    memcpy(&v, p, 8);
    return v;
}
static INLINE bool is_token_byte(u8 b) { return (b >= '0' && b <= '9') || ((b | 0x20) >= 'a' && (b | 0x20) <= 'z'); }

// Hash of a token: its first and last eight lowercased bytes and length, plus the middle for tokens
// over 16 bytes (rare). `| 0x20` lowercases letters and leaves digits alone.
static INLINE u64 token_hash(const u8* p, size_t len) {
    u64 head = (load(p) | LOWER) & (~0ull >> (64 - 8 * std::min<size_t>(len, 8)));
    u64 tail = len > 8 ? load(p + len - 8) | LOWER : 0;
    u64 h = head ^ (tail << 23 | tail >> 41) * 0xC2B2AE3D27D4EB4F ^ len;
    if (len > 16) [[unlikely]]
        for (size_t i = 8; i < len - 8; i++) h = (h ^ (p[i] | 0x20)) * 0x100000001B3;
    h *= 0x9E3779B97F4A7C15;
    return h ^ h >> 29;
}

// Bits set at token bytes and at LFs, for the 64 bytes at `p`.
static INLINE void classify(const u8* p, u64& token, u64& lf) {
#ifdef __AVX2__
    auto half = [](const u8* q, u64& t, u64& l) {
        __m256i v = _mm256_loadu_si256((const __m256i*)q);
        // Shift each class up to the top of the signed range; one compare then tests both ends.
        __m256i lower = _mm256_or_si256(v, _mm256_set1_epi8(0x20));
        __m256i letters = _mm256_cmpgt_epi8(_mm256_add_epi8(lower, _mm256_set1_epi8(5)), _mm256_set1_epi8(0x65));
        __m256i digits = _mm256_cmpgt_epi8(_mm256_add_epi8(v, _mm256_set1_epi8(0x46)), _mm256_set1_epi8(0x75));
        t = (u32)_mm256_movemask_epi8(_mm256_or_si256(letters, digits));
        l = (u32)_mm256_movemask_epi8(_mm256_cmpeq_epi8(v, _mm256_set1_epi8('\n')));
    };
    u64 t0, l0, t1, l1;
    half(p, t0, l0), half(p + 32, t1, l1);
    token = t0 | t1 << 32, lf = l0 | l1 << 32;
#else
    token = lf = 0;
    for (int i = 0; i < 64; i++) token |= (u64)is_token_byte(p[i]) << i, lf |= (u64)(p[i] == '\n') << i;
#endif
}

// Calls f(start, end) for every token of the document text at `p`; returns the byte after its LF.
template <class F>
static INLINE const u8* for_each_token(const u8* p, F f) {
    const u8* open = nullptr;
    for (;;) {
        u64 token, lf;
        classify(p, token, lf);
        const bool ends = lf != 0;
        if (ends) token &= (1ull << __builtin_ctzll(lf)) - 1;
        // Each set bit is a boundary: starts and ends alternate.
        u64 edges = token ^ (token << 1 | (open != nullptr));
        for (; edges; edges &= edges - 1) {
            const u8* at = p + __builtin_ctzll(edges);
            if (!open) {
                open = at;
            } else {
                f(open, at);
                open = nullptr;
            }
        }
        if (ends) {
            const u8* lf_at = p + __builtin_ctzll(lf);
            if (open) f(open, lf_at);  // A token running up to a LF in the block's last byte.
            return lf_at + 1;
        }
        p += 64;
    }
}

struct Word {
    const u8* p;
    u32 len;
};

// Every distinct query term, addressed by a hash of the token. One u64 per slot, a table of 1 MB,
// because every token probes it and it must stay in a core's 1.25 MB L2. A slot keeps 46 more hash
// bits beside term + 1, so a match means 63 bits agree: about 5e-9 chance of any false match in a run.
struct Terms {
    std::vector<u64> slots = std::vector<u64>(1 << TABLE_BITS);
    std::vector<Word> text;
    u32 intern(const u8* p, size_t len) {
        u64 h = token_hash(p, len), want = h << TERM_BITS >> TERM_BITS;
        for (size_t i = h >> (64 - TABLE_BITS);; i = (i + 1) & (slots.size() - 1)) {
            if (!slots[i]) {
                text.push_back({p, (u32)len});
                slots[i] = h << TERM_BITS | text.size();
                return text.size() - 1;
            }
            if (slots[i] >> TERM_BITS == want) return (slots[i] & ((1u << TERM_BITS) - 1)) - 1;
        }
    }
    INLINE u32 find(const u8* p, size_t len) const {
        u64 h = token_hash(p, len), want = h << TERM_BITS >> TERM_BITS;
        for (size_t i = h >> (64 - TABLE_BITS);; i = (i + 1) & (slots.size() - 1)) {
            u64 slot = slots[i];
            if (slot >> TERM_BITS == want && slot) return (slot & ((1u << TERM_BITS) - 1)) - 1;
            if (!slot) return NONE;
        }
    }
};

enum Op : u8 { AND, OR, NOT };
struct BoolOp {
    Op op;
    u32 term;
};
struct BoolQuery {
    u32 query, first, count;
};
// A phrase: its terms in `phrase_terms`, plus which of them to look for in the text.
struct Phrase {
    u32 query, first, count, anchor_at;
};

struct Queries {
    Terms terms;
    std::vector<BoolOp> bool_ops;
    std::vector<BoolQuery> bools;
    std::vector<Phrase> phrases;
    std::vector<u32> phrase_terms;
    size_t total = 0;
};

static void parse_queries(Queries& q, const u8* text, size_t len) {
    for (const u8* p = text, *stop = text + len; p < stop;) {
        const u8* line_end = (const u8*)memchr(p, '\n', stop - p);
        u32 index = q.total++;
        if (p[0] == 'B') {
            u32 first = q.bool_ops.size();
            Op op = OR;
            p += 5;
            for (int i = 0; p < line_end; i++) {
                const u8* w = p;
                while (w < line_end && *w != ' ') w++;
                if (i % 2 == 0) q.bool_ops.push_back({op, q.terms.intern(p, w - p)});
                else op = p[0] == 'A' ? AND : p[0] == 'O' ? OR : NOT;
                p = w + 1;
            }
            q.bools.push_back({index, first, (u32)q.bool_ops.size() - first});
        } else {
            u32 first = q.phrase_terms.size();
            p += 8;
            const u8* end = line_end - 1;
            while (p < end) {
                const u8* w = p;
                while (w < end && *w != ' ') w++;
                q.phrase_terms.push_back(q.terms.intern(p, w - p));
                p = w + 1;
            }
            q.phrases.push_back({index, first, (u32)q.phrase_terms.size() - first, 0});
        }
        p = line_end + 1;
    }
    if (q.total > 1 << 16) abort();
}

// Tables derived from the queries, shared by all workers.
struct Plan {
    std::vector<u32> role;  // Per term: flags and its dense index among the BOOL terms.
    size_t bool_terms = 0, frequent = 0;  // BOOL terms below `frequent` use their index as bitset row.
    std::vector<u32> anchored_at, anchored;  // Per term: its range of phrases anchored on it.
};

static void make_plan(Queries& q, Plan& plan, const std::vector<u32>& frequency) {
    const size_t n = q.terms.text.size();
    plan.role.assign(n, 0);
    u32 bool_terms = 0;
    for (int pass = 1; pass >= 0; pass--) {
        for (const BoolOp& op : q.bool_ops)
            if (!plan.role[op.term] && (frequency[op.term] >= FREQUENT) == (bool)pass) plan.role[op.term] = BOOL_TERM | bool_terms++;
        if (pass) plan.frequent = bool_terms;
    }
    plan.bool_terms = bool_terms;
    for (BoolOp& op : q.bool_ops) op.term = plan.role[op.term] & ~(BOOL_TERM | ANCHOR_TERM);
    plan.anchored_at.assign(n + 1, 0);
    for (Phrase& ph : q.phrases) {
        // Rarest in the sample; among terms it never saw, the longest word is the best guess.
        u32 best = 0;
        for (u32 i = 1; i < ph.count; i++) {
            u32 a = q.phrase_terms[ph.first + i], b = q.phrase_terms[ph.first + best];
            if (frequency[a] < frequency[b] || (frequency[a] == frequency[b] && q.terms.text[a].len > q.terms.text[b].len)) best = i;
        }
        ph.anchor_at = best;
        u32 anchor = q.phrase_terms[ph.first + best];
        plan.role[anchor] |= ANCHOR_TERM;
        plan.anchored_at[anchor + 1]++;
    }
    for (size_t i = 0; i < n; i++) plan.anchored_at[i + 1] += plan.anchored_at[i];
    plan.anchored.resize(q.phrases.size());
    std::vector<u32> cursor = plan.anchored_at;
    for (size_t i = 0; i < q.phrases.size(); i++) plan.anchored[cursor[q.phrase_terms[q.phrases[i].first + q.phrases[i].anchor_at]]++] = i;
}

// Whether the phrase is in the text with its anchor term at `at`, reading outward from there.
static bool phrase_at(const Queries& q, const Phrase& ph, const u8* at) {
    auto same = [&](const u8* p, size_t len, u32 t) {
        const Word& w = q.terms.text[t];
        if (w.len != len) return false;
        for (size_t i = 0; i < len; i++)
            if ((p[i] | (p[i] >= 'A' ? 0x20 : 0)) != w.p[i]) return false;
        return true;
    };
    const u32* terms = q.phrase_terms.data() + ph.first;
    const u8* p = at;
    for (u32 i = ph.anchor_at; i-- > 0;) {
        // Back over separators to the end of the previous token; TAB or LF means the text ended.
        for (;;) {
            p--;
            if (is_token_byte(*p)) break;
            if (*p == '\t' || *p == '\n') return false;
        }
        const u8* end = p + 1;
        while (is_token_byte(p[-1])) p--;
        if (!same(p, end - p, terms[i])) return false;
    }
    p = at + q.terms.text[terms[ph.anchor_at]].len;
    for (u32 i = ph.anchor_at + 1; i < ph.count; i++) {
        while (!is_token_byte(*p)) {
            if (*p == '\n') return false;
            p++;
        }
        const u8* start = p;
        while (is_token_byte(*p)) p++;
        if (!same(start, p - start, terms[i])) return false;
    }
    return true;
}

struct Shared {
    const Queries& q;
    const Plan& plan;
    std::vector<std::atomic<u32>> counts;
    std::vector<std::atomic<u64>> sums;
};

// One chunk's first-ten candidates: query << 16 | relative document, ascending by query.
struct ChunkFirsts {
    u32 base = 0;
    std::vector<u32> entries;
};

static INLINE void set_bit(u64* words, u32 at) { words[at >> 6] |= 1ull << (at & 63); }

// One worker's reusable buffers.
struct Worker {
    std::vector<u16> last_doc, list;
    std::vector<u32> log, counts, dense_of, last_hit;
    std::vector<u64> dense, result, scratch;
    std::vector<u8> emitted, buffer;
    std::vector<std::pair<u32, u32>> hits;

    Worker(const Queries& q, const Plan& plan)
        : last_doc(plan.bool_terms), counts(plan.bool_terms + 1), dense_of(plan.bool_terms, NONE), last_hit(q.phrases.size(), NONE), result(STRIDE), scratch(STRIDE), emitted(q.total), buffer(READ_BYTES + 2 * PAD) {
        // Sized once: unlike realloc, a growing vector holds old and new at once and leaves holes.
        log.reserve(1 << 18), list.reserve(1 << 18), dense.reserve(1 << 17);
    }

    // Tokenises file range [start, end) of whole documents; sets the base id and document count.
    // Bytes are read in pieces that stay in this core's L2, and phrases are verified on the spot,
    // while the text around the occurrence is still in the buffer.
    void scan(int fd, size_t start, size_t end, const Shared& s, u32& base, size_t& docs) {
        const Queries& q = s.q;
        const Plan& plan = s.plan;
        log.clear();
        std::fill(last_doc.begin(), last_doc.end(), 0);
        dense.assign(plan.frequent * STRIDE, 0);
        base = 0, docs = 0;
        for (size_t position = start; position < end;) {
            size_t wanted = std::min(READ_BYTES, end - position);
            // A LF in front means reading backwards from the first document's text stops in time.
            buffer[PAD - 1] = '\n';
            if (pread(fd, buffer.data() + PAD, wanted, position) != (ssize_t)wanted) abort();
            size_t whole = wanted;
            while (buffer[PAD + whole - 1] != '\n') whole--;
            const u8* p = buffer.data() + PAD;
            const u8* stop = p + whole;
            while (p < stop) {
                u32 id = 0;
                for (; *p != '\t'; p++) id = id * 10 + (*p - '0');
                if (!docs) base = id;
                docs++;
                const u32 rel = id - base;
                if (rel >> REL_BITS) abort();
                p = for_each_token(p + 1, [&](const u8* a, const u8* b) {
                    u32 term = q.terms.find(a, b - a);
                    if (term == NONE) return;
                    u32 role = plan.role[term];
                    if (role & BOOL_TERM) {
                        u32 index = role & ~(BOOL_TERM | ANCHOR_TERM);
                        if (index < plan.frequent) {
                            dense[index * STRIDE + (rel >> 6)] |= 1ull << (rel & 63);
                        } else if (last_doc[index] != rel + 1) {
                            last_doc[index] = rel + 1;
                            log.push_back(index << REL_BITS | rel);
                        }
                    }
                    if (role & ANCHOR_TERM)
                        for (u32 i = plan.anchored_at[term]; i < plan.anchored_at[term + 1]; i++) {
                            u32 index = plan.anchored[i];
                            const Phrase& ph = q.phrases[index];
                            if (last_hit[index] != id && phrase_at(q, ph, a)) {
                                last_hit[index] = id;
                                hits.push_back({ph.query, id});
                            }
                        }
                });
            }
            position += whole;
        }
    }

    // Groups the log by term: each term's documents become a u16 list, or a bitset if it has enough
    // of them that word-at-a-time operations win.
    void invert(size_t words, size_t frequent) {
        std::fill(counts.begin(), counts.end(), 0);
        for (u32 e : log) counts[(e >> REL_BITS) + 1]++;
        u32 total = 0;
        for (size_t t = 0; t < dense_of.size(); t++) {
            u32 count = counts[t + 1];
            dense_of[t] = NONE;
            if (t < frequent) {
                dense_of[t] = t;
            } else if (count >= words) {
                dense_of[t] = dense.size() / STRIDE;
                dense.resize(dense.size() + STRIDE, 0);
            }
            counts[t + 1] = total;
            total += count;
        }
        list.resize(total);
        for (u32 e : log) {
            u32 t = e >> REL_BITS, rel = e & ((1u << REL_BITS) - 1);
            if (dense_of[t] != NONE) set_bit(&dense[dense_of[t] * STRIDE], rel);
            list[counts[t + 1]++] = rel;
        }
    }

    // Evaluates one BOOL query on this chunk into `result`, strictly left to right.
    bool evaluate(const BoolOp* ops, size_t n, size_t words) {
        // `result` is cleared only once some term can make it non-empty: most queries have no term
        // in a given chunk, and they should cost almost nothing.
        bool any = false;
        for (size_t i = 0; i < n; i++) {
            const u32 t = ops[i].term;
            if (!any && ops[i].op != OR) continue;
            const u16* docs = list.data() + (t ? counts[t] : 0);
            const size_t count = list.data() + counts[t + 1] - docs;
            const u32 at = dense_of[t];
            const u64* bits = at != NONE ? &dense[at * STRIDE] : nullptr;
            u64* r = result.data();
            if (ops[i].op == OR) {
                if (!any) {
                    if (!bits && !count) continue;
                    std::fill_n(r, words, 0);
                    any = true;
                }
                if (bits) {
                    for (size_t w = 0; w < words; w++) r[w] |= bits[w];
                } else {
                    for (size_t d = 0; d < count; d++) set_bit(r, docs[d]);
                }
            } else if (ops[i].op == AND && any) {
                if (bits) {
                    for (size_t w = 0; w < words; w++) r[w] &= bits[w];
                } else {
                    u64* out = scratch.data();
                    std::fill_n(out, words, 0);
                    for (size_t d = 0; d < count; d++)
                        if (r[docs[d] >> 6] >> (docs[d] & 63) & 1) set_bit(out, docs[d]);
                    result.swap(scratch);
                }
            } else if (ops[i].op == NOT && any && (bits || count)) {
                if (bits) {
                    for (size_t w = 0; w < words; w++) r[w] &= ~bits[w];
                } else {
                    for (size_t d = 0; d < count; d++) r[docs[d] >> 6] &= ~(1ull << (docs[d] & 63));
                }
            }
        }
        return any;
    }

    // Answers every BOOL query on the chunk.
    void answer_bools(u32 base, size_t words, Shared& s, std::vector<u32>& out) {
        const Queries& q = s.q;
        for (const BoolQuery& b : q.bools) {
            if (!evaluate(q.bool_ops.data() + b.first, b.count, words)) continue;
            u64 matches = 0, sum = 0;
            u32 quota = 10 - emitted[b.query];
            for (size_t w = 0; w < words; w++) {
                const u64 word = result[w];
                if (!word) continue;
                const u64 n = __builtin_popcountll(word);
                matches += n;
                // The sum of a word's bit positions, one popcount per bit of the position.
                u64 positions = __builtin_popcountll(word & 0xAAAAAAAAAAAAAAAA) + ((u64)__builtin_popcountll(word & 0xCCCCCCCCCCCCCCCC) << 1) + ((u64)__builtin_popcountll(word & 0xF0F0F0F0F0F0F0F0) << 2) + ((u64)__builtin_popcountll(word & 0xFF00FF00FF00FF00) << 3) + ((u64)__builtin_popcountll(word & 0xFFFF0000FFFF0000) << 4) + ((u64)__builtin_popcountll(word & 0xFFFFFFFF00000000) << 5);
                sum += n * (base + 64 * w) + positions;
                for (u64 rest = word; quota && rest; rest &= rest - 1, quota--) out.push_back(b.query << 16 | (w * 64 + __builtin_ctzll(rest)));
            }
            if (matches) {
                emitted[b.query] = 10 - quota;
                s.counts[b.query].fetch_add(matches, std::memory_order_relaxed);
                s.sums[b.query].fetch_add(sum, std::memory_order_relaxed);
            }
        }
    }
};

// First document start at or after `from`.
static size_t doc_start(int fd, size_t len, size_t from) {
    if (from == 0 || from >= len) return std::min(from, len);
    u8 probe[1 << 14];  // Documents are far shorter.
    ssize_t got = pread(fd, probe, sizeof probe, from);
    for (ssize_t i = 0; i < got; i++)
        if (probe[i] == '\n') return from + i + 1;
    return from + got;
}

static inline char* put(char* out, u64 v) {
    char buf[20];
    int i = 20;
    do buf[--i] = '0' + v % 10;
    while (v /= 10);
    memcpy(out, buf + i, 20 - i);
    return out + (20 - i);
}

int main() {
    prctl(PR_SET_THP_DISABLE, 1, 0, 0, 0);  // Huge pages would round each small arena up to 2 MB.
    const char* d = getenv("SD_DATA");
    const char* o = getenv("SD_OUT");
    const std::string data = d ? d : "/data", out = o ? o : "/out";
    int fd = open((data + "/docs.txt").c_str(), O_RDONLY), qf = open((data + "/queries.txt").c_str(), O_RDONLY);
    struct stat st, qt;
    if (fd < 0 || qf < 0 || fstat(fd, &st) || fstat(qf, &qt)) abort();
    const size_t len = st.st_size;
    u8* qtext = (u8*)malloc(qt.st_size + 16);
    if (read(qf, qtext, qt.st_size) != qt.st_size) abort();
    static Queries q;
    parse_queries(q, qtext, qt.st_size);

    // How often each term occurs at the start of the corpus, to tell rare terms from common ones.
    const size_t sample_end = doc_start(fd, len, SAMPLE_BYTES);
    std::vector<u8> sample(sample_end + 2 * PAD);
    if (pread(fd, sample.data(), sample_end, 0) != (ssize_t)sample_end) abort();
    std::vector<u32> frequency(q.terms.text.size());
    for (const u8* p = sample.data(), *stop = p + sample_end; p < stop;) {
        while (*p != '\t') p++;
        p = for_each_token(p + 1, [&](const u8* a, const u8* b) {
            u32 term = q.terms.find(a, b - a);
            if (term != NONE) frequency[term]++;
        });
    }
    static Plan plan;
    make_plan(q, plan, frequency);

    const size_t threads = std::clamp<size_t>(std::thread::hardware_concurrency(), 1, 12);
    const size_t chunks = (len + CHUNK_BYTES - 1) / CHUNK_BYTES;
    Shared shared{q, plan, std::vector<std::atomic<u32>>(q.total), std::vector<std::atomic<u64>>(q.total)};
    std::vector<ChunkFirsts> firsts(chunks);
    std::vector<std::vector<std::pair<u32, u32>>> hit_lists(threads);
    std::atomic<size_t> claim{0};
    {
        std::vector<std::jthread> pool;
        for (size_t t = 0; t < threads; t++)
            pool.emplace_back([&, t] {
                Worker w(q, plan);
                for (size_t i; (i = claim.fetch_add(1, std::memory_order_relaxed)) < chunks;) {
                    size_t docs;
                    u32 base;
                    w.scan(fd, doc_start(fd, len, i * CHUNK_BYTES), doc_start(fd, len, (i + 1) * CHUNK_BYTES), shared, base, docs);
                    const size_t words = std::max<size_t>((docs + 63) / 64, 1);
                    w.invert(words, plan.frequent);
                    firsts[i].base = base;
                    w.answer_bools(base, words, shared, firsts[i].entries);
                    firsts[i].entries.shrink_to_fit();
                }
                hit_lists[t] = std::move(w.hits);
            });
    }
    // Chunks in ascending order give each query its smallest ids first.
    std::vector<u32> first_ten(q.total * 10);
    std::vector<u8> filled(q.total);
    for (const ChunkFirsts& c : firsts)
        for (u32 e : c.entries) {
            u32 query = e >> 16;
            if (filled[query] < 10) first_ten[query * 10 + filled[query]++] = c.base + (e & 0xFFFF);
        }
    std::vector<u64> counts(q.total), sums(q.total);
    for (size_t i = 0; i < q.total; i++) counts[i] = shared.counts[i].load(), sums[i] = shared.sums[i].load();
    std::vector<std::pair<u32, u32>> hits;
    for (auto& l : hit_lists) hits.insert(hits.end(), l.begin(), l.end());
    std::sort(hits.begin(), hits.end());
    hits.erase(std::unique(hits.begin(), hits.end()), hits.end());
    for (auto [query, doc] : hits) {
        counts[query]++, sums[query] += doc;
        if (filled[query] < 10) first_ten[query * 10 + filled[query]++] = doc;
    }
    char* const text = (char*)malloc(q.total * 120 + 1);  // Worst case, and untouched pages cost nothing.
    char* w = text;
    for (size_t i = 0; i < q.total; i++) {
        w = put(w, counts[i]);
        *w++ = ' ';
        w = put(w, sums[i]);
        for (u32 k = 0; k < filled[i]; k++) {
            *w++ = ' ';
            w = put(w, first_ten[i * 10 + k]);
        }
        *w++ = '\n';
    }
    int output = open((out + "/results.txt").c_str(), O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (output < 0 || write(output, text, w - text) != w - text) abort();
    _exit(0);  // Skip unmapping and destructors: the process is done and the clock is running.
}
