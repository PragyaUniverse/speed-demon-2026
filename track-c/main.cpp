// Track C: hostile CSV to per-country aggregates.
//
// The parser walks one field at a time with branch-free word tricks. Workers can start anywhere
// because a record start is recognisable without tracking quote state: the generator's text is
// letters and punctuation only (spec 2.2), so an LF followed by a digit or a BOM is always a real
// record boundary, and an LF inside a quoted note never is.
#include <fcntl.h>
#ifdef __x86_64__
#include <immintrin.h>
#endif
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <unistd.h>

#include <algorithm>
#include <atomic>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

using u8 = uint8_t;
using u32 = uint32_t;
using u64 = uint64_t;
using i64 = int64_t;

constexpr size_t SEGMENTS = 64, BATCH = 128;
constexpr u32 ID_BITS = 18;  // Name table slots; the pool holds 100,000 names.
constexpr u64 ID_MASK = (1ull << ID_BITS) - 1;
constexpr u64 LOW = 0x0101010101010101, HIGH = 0x8080808080808080;

static inline u64 load(const u8* p) {
    u64 v;
    memcpy(&v, p, 8);
    return v;
}
// Byte index of the first zero byte of the little-endian word, 8 if there is none.
static inline size_t first_zero(u64 w) {
    u64 m = (w - LOW) & ~w & HIGH;
    return m ? __builtin_ctzll(m) / 8 : 8;
}
static inline size_t first_byte(u64 w, u8 b) { return first_zero(w ^ (b * LOW)); }
// Combines up to 8 ASCII digits sitting in the high bytes of the word, one multiply per level.
static inline u64 digits8(u64 w) {
    u64 v = w & 0x0F0F0F0F0F0F0F0F;
    v = (v * 2561 >> 8) & 0x00FF00FF00FF00FF;
    v = (v * 6553601 >> 16) & 0x0000FFFF0000FFFF;
    return v * 42949672960001 >> 32;
}
// Distance to the next `b`, which is known to exist.
static inline size_t until(const u8* p, u8 b) {
#ifdef __AVX2__
    const __m256i needle = _mm256_set1_epi8(b);
    for (size_t n = 0;; n += 32) {
        u32 m = _mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256((const __m256i*)(p + n)), needle));
        if (m) return n + __builtin_ctz(m);
    }
#else
    return (const u8*)memchr(p, b, SIZE_MAX) - p;
#endif
}
// Multiply-mix over all bytes of the name, with a masked load so that nothing past it counts.
static inline u64 hash_name(const u8* p, size_t len) {
    constexpr u64 K = 0x9E3779B97F4A7C15;
    u64 h = len * K;
    size_t i = 0;
    for (; i + 8 < len; i += 8) {
        h = (h ^ load(p + i)) * K;
        h = h << 29 | h >> 35;
    }
    if (len) h = (h ^ (load(p + i) & (~0ull >> (64 - 8 * (len - i))))) * K;
    return h ^ h >> 32;
}

struct Group {
    u64 count = 0, id_sum = 0;
    i64 sum = 0, min = INT64_MAX, max = INT64_MIN;
    u64 name_bytes = 0, note_bytes = 0;
};

// Maps a file with zeroed space behind it, so word reads may run past the end of the data.
static const u8* map_file(const std::string& path, size_t& len) {
    int fd = open(path.c_str(), O_RDONLY);
    struct stat st;
    if (fd < 0 || fstat(fd, &st)) abort();
    len = st.st_size;
    u8* base = (u8*)mmap(nullptr, len + 4096, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (len && mmap(base, len, PROT_READ, MAP_PRIVATE | MAP_FIXED, fd, 0) != base) abort();
    return base;
}
template <class T>
static T* lazy_table(size_t n) {
    return (T*)mmap(nullptr, n * sizeof(T), PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
}

// State shared by the workers while they parse.
struct Shared {
    std::vector<u8> region_group;  // One extra zero at the end catches every larger region_id.
    std::atomic<u64>* names;       // High hash bits | id + 1; the slot index holds the low bits.
    std::atomic<u32> next_name{0};
    u64* seen;  // One row of `stride` words per 64 name ids; each cache line has one owning worker.
    size_t stride = 0;
    std::vector<std::mutex> locks;
    std::vector<std::vector<u32>> inboxes;
    std::atomic<size_t> finished{0};
    explicit Shared(size_t workers) : locks(workers), inboxes(workers) {}

    inline u32 name_id(u64 hash) {
        const u64 tag = hash & ~ID_MASK;
        size_t slot = hash & ID_MASK;
        for (;;) {
            u64 e = names[slot].load(std::memory_order_relaxed);
            if ((e & ~ID_MASK) == tag && e) return (e & ID_MASK) - 1;
            if (!e) {
                u64 id = next_name.fetch_add(1, std::memory_order_relaxed);
                if (id >= ID_MASK) abort();
                u64 expected = 0;
                if (names[slot].compare_exchange_strong(expected, tag | (id + 1), std::memory_order_relaxed)) return id;
                continue;  // Lost the race: the winner may have stored this very name.
            }
            slot = (slot + 1) & ID_MASK;
        }
    }
};

// One worker's aggregates, plus its (name id, group) pairs on their way to the lines' owners.
struct Worker {
    size_t me;
    std::vector<Group> groups;
    std::vector<u32> lens, batches, spare;
    Worker(size_t me, size_t count) : me(me), groups(count), lens(count / 8 + 1), batches((count / 8 + 1) * BATCH) {}

    inline void mark_name(Shared& s, size_t group, u32 id) {
        size_t line = group >> 3;
        u32& len = lens[line];
        batches[line * BATCH + len++] = id << 8 | group;
        if (len == BATCH) {
            len = 0;
            send(s, line, BATCH);
            drain(s);
        }
    }
    void send(Shared& s, size_t line, size_t n) {
        size_t owner = line % s.inboxes.size();
        const u32* from = &batches[line * BATCH];
        std::lock_guard<std::mutex> guard(s.locks[owner]);
        s.inboxes[owner].insert(s.inboxes[owner].end(), from, from + n);
    }
    // Applies everything sent to this worker. Only the owner writes these lines.
    void drain(Shared& s) {
        {
            std::lock_guard<std::mutex> guard(s.locks[me]);
            s.inboxes[me].swap(spare);
        }
        for (u32 e : spare) {
            u32 id = e >> 8, group = e & 255;
            s.seen[(id >> 6) * s.stride + group] |= 1ull << (id & 63);
        }
        spare.clear();
    }
    void finish(Shared& s) {
        for (size_t line = 0; line < lens.size(); line++) send(s, line, lens[line]);
        s.finished.fetch_add(1, std::memory_order_acq_rel);
        while (s.finished.load(std::memory_order_acquire) < s.inboxes.size()) {
            drain(s);
            std::this_thread::yield();
        }
        drain(s);
    }
};

// A text field: raw content and unescaped length. `""` is the only escape, and unquoted text cannot
// contain a quote, so raw bytes compare exactly as values do.
static inline const u8* text_field(const u8* p, const u8*& start, size_t& raw, size_t& value) {
    if (*p != '"') {
        size_t n = until(p, ',');
        start = p, raw = value = n;
        return p + n + 1;
    }
    start = p + 1;
    const u8* q = start;
    size_t escapes = 0;
    for (;;) {
        q += until(q, '"');
        if (q[1] != '"') break;
        q += 2, escapes++;
    }
    raw = q - start, value = raw - escapes;
    return q + 2;
}

static void parse_records(const u8* input, size_t begin, size_t end, Shared& s, Worker& w) {
    const u8* regions = s.region_group.data();
    const u64 last_region = s.region_group.size() - 1;
    const u8* p = input + begin;
    const u8* stop = input + end;
    while (p < stop) {
        if (*p == 0xEF) p += 3;
        u64 word = load(p);
        size_t k = first_byte(word, ',');
        u64 id = digits8(word << (64 - 8 * k));
        p += k + 1;
        const u8* name;
        const u8* note;
        size_t name_raw, name_len, note_raw, note_len;
        p = text_field(p, name, name_raw, name_len);
        p = text_field(p, note, note_raw, note_len);
        word = load(p);
        k = first_byte(word, ',');
        u64 region = digits8(word << (64 - 8 * k));
        p += k + 1;
        // Amount: -?digits.dd of at most 8 characters after the sign, then CR or LF. Only those
        // terminators have a zero high nibble. Sign and CR are unpredictable, so no branches.
        size_t negative = *p == '-';
        p += negative;
        word = load(p);
        k = first_zero(word & 0xF0F0F0F0F0F0F0F0);
        u64 aligned = word << (64 - 8 * k);  // Bytes 6 and 7 are the cents, byte 5 is the dot.
        i64 value = digits8(((aligned & 0xFFFFFFFFFF) << 8) | (aligned & 0xFFFF000000000000));
        i64 amount = (value ^ -(i64)negative) + (i64)negative;
        p += k + 1 + (p[k] == '\r');

        size_t group = regions[std::min(region, last_region)];
        Group& g = w.groups[group];
        g.count++;
        g.id_sum += id;
        g.sum += amount;
        g.min = std::min(g.min, amount);
        g.max = std::max(g.max, amount);
        g.name_bytes += name_len;
        g.note_bytes += note_len;
        w.mark_name(s, group, s.name_id(hash_name(name, name_raw)));
    }
}

// First record start at or after `from`, per the comment at the top.
static size_t record_start(const u8* input, size_t len, size_t from) {
    for (; from < len; from++)
        if (input[from] == '\n' && from + 1 < len && ((input[from + 1] >= '0' && input[from + 1] <= '9') || input[from + 1] == 0xEF))
            return from + 1;
    return len;
}

static std::string money(i64 cents) {
    u64 a = cents < 0 ? -(u64)cents : cents;
    char buf[32];
    snprintf(buf, sizeof buf, "%s%llu.%02llu", cents < 0 ? "-" : "", (unsigned long long)(a / 100), (unsigned long long)(a % 100));
    return buf;
}

int main() {
    prctl(PR_SET_THP_DISABLE, 1, 0, 0, 0);  // Huge pages would round each small arena up to 2 MB.
    const char* d = getenv("SD_DATA");
    const char* o = getenv("SD_OUT");
    const std::string data = d ? d : "/data", out = o ? o : "/out";
    size_t len, region_len;
    const u8* input = map_file(data + "/records.csv", len);
    const u8* rtext = map_file(data + "/regions.csv", region_len);

    // regions.csv: ids ascend, so the last line sizes the flat table.
    size_t last = region_len - 1;
    while (last > 0 && rtext[last - 1] != '\n') last--;
    size_t max_id = 0;
    for (size_t i = last; rtext[i] != ','; i++) max_id = max_id * 10 + (rtext[i] - '0');
    std::vector<std::string> codes{"_UNMATCHED"};
    static u8 group_of_code[1 << 15];
    size_t threads = std::min<size_t>(12, std::max(1u, std::thread::hardware_concurrency()));
    Shared shared(threads);
    shared.region_group.assign(max_id + 2, 0);
    for (const u8* p = rtext + until(rtext, '\n') + 1, *stop = rtext + region_len; p < stop;) {
        u64 word = load(p);
        size_t k = first_byte(word, ',');
        u64 id = digits8(word << (64 - 8 * k));
        p += k + 1;
        size_t key = (p[0] & 31) << 10 | (p[1] & 31) << 5 | (p[2] & 31);
        if (!group_of_code[key]) {
            codes.emplace_back((const char*)p, 3);
            if (codes.size() > 256) abort();
            group_of_code[key] = codes.size() - 1;
        }
        shared.region_group[id] = group_of_code[key];
        p += 4;
    }
    const size_t count = codes.size(), stride = (count + 7) / 8 * 8;
    shared.stride = stride;
    shared.names = lazy_table<std::atomic<u64>>(1ull << ID_BITS);
    shared.seen = lazy_table<u64>((1ull << (ID_BITS - 6)) * stride);

    size_t first = len ? until(input, '\n') + 1 : 0;
    auto cut = [&](size_t i) {
        if (i == 0) return first;
        if (i >= SEGMENTS) return len;
        return record_start(input, len, std::max(first, first + i * (len - first) / SEGMENTS - 1));
    };
    std::atomic<size_t> claim{0};
    std::vector<std::vector<Group>> results(threads);
    std::vector<std::thread> pool;
    for (size_t me = 0; me < threads; me++)
        pool.emplace_back([&, me] {
            Worker w(me, count);
            for (;;) {
                size_t i = claim.fetch_add(1, std::memory_order_relaxed);
                if (i >= SEGMENTS) break;
                parse_records(input, cut(i), cut(i + 1), shared, w);
            }
            w.finish(shared);
            results[me] = std::move(w.groups);
        });
    for (auto& t : pool) t.join();

    std::vector<Group> totals(count);
    for (auto& r : results)
        for (size_t g = 0; g < count; g++) {
            Group& t = totals[g];
            t.count += r[g].count, t.id_sum += r[g].id_sum, t.sum += r[g].sum;
            t.min = std::min(t.min, r[g].min), t.max = std::max(t.max, r[g].max);
            t.name_bytes += r[g].name_bytes, t.note_bytes += r[g].note_bytes;
        }
    size_t rows = (shared.next_name.load() >> 6) + 1;
    std::vector<std::pair<std::string, std::string>> lines;
    for (size_t g = 0; g < count; g++) {
        const Group& t = totals[g];
        if (!t.count) continue;
        u64 distinct = 0;
        for (size_t r = 0; r < rows; r++) distinct += __builtin_popcountll(shared.seen[r * stride + g]);
        std::string line = codes[g] + " " + std::to_string(t.count) + " " + std::to_string(t.id_sum) + " " + money(t.sum) + " " + money(t.min) + " " + money(t.max) + " " + std::to_string(distinct) + " " + std::to_string(t.name_bytes) + " " + std::to_string(t.note_bytes) + "\n";
        lines.emplace_back(codes[g], std::move(line));
    }
    std::sort(lines.begin(), lines.end());
    std::string text;
    for (auto& l : lines) text += l.second;
    int fd = open((out + "/aggregates.txt").c_str(), O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd < 0 || write(fd, text.data(), text.size()) != (ssize_t)text.size()) abort();
    _exit(0);  // Skip unmapping and destructors: the process is done and the clock is running.
}
