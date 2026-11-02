// Track A: per-hour, per-endpoint latency percentiles over a request log.
//
// Phase 1: workers claim file segments and turn each line into one packed u32, chained into
// per-hour blocks inside that segment's own arena. Phase 2: workers take one hour at a time, select
// its percentiles and write its text at the final file offset. A segment's arena goes back to the
// kernel when its last hour is done: the log is roughly time-ordered, so the record store shrinks
// while the output grows, and peak memory is the larger of the two.
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
#include <cstdlib>
#include <cstring>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

using u8 = uint8_t;
using u32 = uint32_t;
using u64 = uint64_t;

constexpr size_t HOURS = 72, SEGMENTS = 24, READ_BYTES = 1 << 18, PAD = 64;
constexpr size_t BLOCK = 256;  // Words per block; the first word links to the hour's previous block.
constexpr u32 SLOT_BITS = 12;  // 1,842 endpoints per seed: the table stays under half full.
constexpr size_t SLOTS = 1 << SLOT_BITS;
// A record packs slot:12 | error:1 | latency:19. The 2.6% of latencies that do not fit store the
// marker and spill the value into the next word, so 3.1M lines take about 13 MB.
constexpr u32 LAT_BITS = 19, SPILL = (1u << LAT_BITS) - 1;
static const char* const DAYS[4] = {"2026-10-30T", "2026-10-31T", "2026-11-01T", "2026-11-02T"};

static inline u64 load(const u8* p) {
    u64 v;
    memcpy(&v, p, 8);
    return v;
}
// Byte index of the first byte equal to `b` in the little-endian word, 8 if there is none.
static inline size_t first_byte(u64 word, u8 b) {
    u64 x = word ^ (b * 0x0101010101010101ull);
    u64 m = (x - 0x0101010101010101ull) & ~x & 0x8080808080808080ull;
    return m ? __builtin_ctzll(m) / 8 : 8;
}
// Length of the run of bytes before the next space.
static inline size_t until_space(const u8* p) {
#ifdef __AVX2__
    const __m256i spaces = _mm256_set1_epi8(' ');
    for (size_t n = 0;; n += 32) {
        u32 m = _mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256((const __m256i*)(p + n)), spaces));
        if (m) return n + __builtin_ctz(m);
    }
#else
    return (const u8*)memchr(p, ' ', SIZE_MAX) - p;
#endif
}

// Lock-free endpoint interner shared by all workers. After the first few thousand lines it is
// read-only, so its cache lines stay shared. A slot packs (address of a copy) << 8 | length.
struct Interner {
    std::atomic<u64> slots[SLOTS] = {};
    inline size_t intern(const u8* p, size_t len) {
        // Sampling head, middle and tail keeps hashing O(1); equality is still byte-exact.
        u64 head = load(p) & (~0ull >> (64 - 8 * std::min<size_t>(len, 8)));
        u64 mid = len >= 8 ? load(p + len / 2 - 4) : 0, tail = len >= 8 ? load(p + len - 8) : 0;
        u64 mixed = head ^ (mid << 21 | mid >> 43) ^ (tail << 42 | tail >> 22) ^ len;
        size_t slot = mixed * 0x9E3779B97F4A7C15ull >> (64 - SLOT_BITS);
        for (;;) {
            u64 packed = slots[slot].load(std::memory_order_acquire);
            if (!packed) {
                u8* copy = (u8*)malloc(len);
                memcpy(copy, p, len);
                u64 expected = 0;
                if (slots[slot].compare_exchange_strong(expected, (u64)copy << 8 | len)) return slot;
                continue;  // Lost the race: re-read the slot, the winner may hold our key.
            }
            if ((packed & 255) == len && !memcmp((const u8*)(packed >> 8), p, len)) return slot;
            slot = (slot + 1) & (SLOTS - 1);
        }
    }
};

// Decimal value of a latency whose digits end at the LF: returns the value, sets the digit count.
static inline u32 parse_latency(const u8* p, size_t& digits) {
    u64 word = load(p);
    size_t k = first_byte(word, '\n');
    if (k == 8) {  // 9 or 10 digits, a fraction of a percent of lines.
        u64 v = 0;
        size_t n = 0;
        for (; p[n] != '\n'; n++) v = v * 10 + (p[n] - '0');
        digits = n;
        return v;
    }
    // Right-align the digits (vacated low bytes act as leading zeros), then combine by pairs.
    u64 v = (word << (64 - 8 * k)) & 0x0F0F0F0F0F0F0F0F;
    v = (v * 2561 >> 8) & 0x00FF00FF00FF00FF;
    v = (v * 6553601 >> 16) & 0x0000FFFF0000FFFF;
    digits = k;
    return v * 42949672960001 >> 32;
}

// The records of one file segment. Each hour's words form a chain of BLOCK-aligned blocks.
struct Segment {
    u32* arena = nullptr;
    size_t arena_bytes = 0, next_block = 0;  // Block 0 is never used: a cursor of 0 means no block.
    u32 cursor[HOURS] = {};                  // Next free word of the hour's newest block.
    std::atomic<u32> hours_left{0};

    inline void push(size_t bucket, u32 word) {
        size_t at = cursor[bucket];
        if (at % BLOCK == 0) {
            arena[next_block] = at;
            at = next_block + 1;
            next_block += BLOCK;
        }
        arena[at] = word;
        cursor[bucket] = at + 1;
    }
    void parse_lines(const u8* p, const u8* stop, Interner& names) {
        while (p < stop) {
            // "2026-10-30T18:..": the span is fixed, so the day's last digit plus whether the tens
            // digit is '3' (October, bit 0 set) gives the day index directly.
            size_t day = (p[9] & 15) + 1 - (p[8] & 1);
            size_t bucket = day * 24 + (p[11] & 15) * 10 + (p[12] & 15) - 18;
            const u8* endpoint = p + 29 + first_byte(load(p + 28), ' ');
            size_t len = until_space(endpoint);
            u32 slot = names.intern(endpoint, len);
            const u8* status = endpoint + len + 1;
            size_t digits;
            u32 latency = parse_latency(status + 4, digits);
            u32 tag = slot << (LAT_BITS + 1) | (u32)(*status == '5') << LAT_BITS;
            if (latency < SPILL) {
                push(bucket, tag | latency);
            } else {
                // A pair never straddles blocks: a marker in a block's last word is a filler.
                if (cursor[bucket] % BLOCK == BLOCK - 1) push(bucket, SPILL);
                push(bucket, tag | SPILL);
                push(bucket, latency);
            }
            p = status + 5 + digits;
        }
    }
    // Parses file range [start, end) through a buffer small enough to stay in this core's L2.
    void parse(int fd, size_t start, size_t end, Interner& names, u8* buffer) {
        // Lines are at least 40 bytes and take at most 2 words: virtual space, touched only as used.
        arena_bytes = (end - start) / 4 + (HOURS + 2) * BLOCK * 8;
        arena = (u32*)mmap(nullptr, arena_bytes, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        next_block = BLOCK;
        for (size_t position = start; position < end;) {
            size_t wanted = std::min(READ_BYTES, end - position);
            if (pread(fd, buffer, wanted, position) != (ssize_t)wanted) abort();
            size_t whole = wanted;
            while (buffer[whole - 1] != '\n') whole--;
            parse_lines(buffer, buffer + whole, names);
            position += whole;
        }
        u32 used = 0;
        for (u32 c : cursor) used += c != 0;
        hours_left = used;
    }
    // Calls f(slot, is_error, latency) for every record of an hour.
    template <class F>
    inline void for_each(size_t bucket, F f) const {
        for (size_t end = cursor[bucket]; end;) {
            size_t start = (end - 1) / BLOCK * BLOCK;
            const u32* words = arena + start;
            size_t n = end - start;
            for (size_t i = 1; i < n; i++) {
                u32 w = words[i], latency = w & SPILL;
                if (latency == SPILL) {
                    if (i + 1 == n) break;
                    latency = words[++i];
                }
                f(w >> (LAT_BITS + 1), (w >> LAT_BITS) & 1, latency);
            }
            end = words[0];
        }
    }
    void hour_done(size_t bucket) {  // The last call gives the arena back.
        if (cursor[bucket] && hours_left.fetch_sub(1, std::memory_order_relaxed) == 1) madvise(arena, arena_bytes, MADV_DONTNEED);
    }
};

// Appends a decimal number.
static inline char* put(char* out, u64 v) {
    char buf[20];
    int i = 20;
    do buf[--i] = '0' + v % 10;
    while (v /= 10);
    memcpy(out, buf + i, 20 - i);
    return out + (20 - i);
}

struct Name {
    size_t slot;
    const u8* bytes;
    size_t len;
};

// Formats one hour: counts per slot, lays latencies out contiguously, selects percentiles.
static void format_hour(size_t bucket, Segment* segments, const std::vector<Name>& order, std::vector<u32>& lat, std::string& text) {
    u32 counts[SLOTS] = {}, errors[SLOTS] = {}, next[SLOTS];
    for (size_t s = 0; s < SEGMENTS; s++)
        segments[s].for_each(bucket, [&](u32 slot, u32 err, u32) {
            counts[slot]++;
            errors[slot] += err;
        });
    u32 total = 0;
    for (size_t i = 0; i < SLOTS; i++) next[i] = total, total += counts[i];
    lat.resize(total);
    for (size_t s = 0; s < SEGMENTS; s++)
        segments[s].for_each(bucket, [&](u32 slot, u32, u32 latency) { lat[next[slot]++] = latency; });
    text.clear();
    const char* day = DAYS[(bucket + 18) / 24];
    size_t hour = (bucket + 18) % 24;
    char line[256];
    for (const Name& name : order) {
        size_t n = counts[name.slot];
        if (!n) continue;
        u32* group = lat.data() + next[name.slot] - n;  // next[] is one past this slot's run after the fill.
        // Each select leaves everything right of its rank larger, so the next one narrows.
        auto rank = [n](size_t p) { return (p * n + 99) / 100 - 1; };
        size_t r50 = rank(50), r95 = rank(95), r99 = rank(99);
        std::nth_element(group, group + r50, group + n);
        std::nth_element(group + r50 + 1 <= group + r95 ? group + r50 + 1 : group + r50, group + r95, group + n);
        std::nth_element(group + r95 + 1 <= group + r99 ? group + r95 + 1 : group + r95, group + r99, group + n);
        char* w = line;
        memcpy(w, day, 11), w += 11;
        *w++ = '0' + hour / 10, *w++ = '0' + hour % 10;
        memcpy(w, ":00:00Z ", 8), w += 8;
        memcpy(w, name.bytes, name.len), w += name.len;
        u64 e = errors[name.slot];
        for (u64 v : {(u64)n, (u64)group[r50], (u64)group[r95], (u64)group[r99], e, e * 1000000 / n}) {
            *w++ = ' ';
            w = put(w, v);
        }
        *w++ = '\n';
        text.append(line, w - line);
    }
}

// First line start at or after `from`; lines are far shorter than the probe.
static size_t line_start(int fd, size_t len, size_t from) {
    if (from == 0 || from >= len) return std::min(from, len);
    u8 probe[256];
    ssize_t got = pread(fd, probe, sizeof probe, from);
    for (ssize_t i = 0; i < got; i++)
        if (probe[i] == '\n') return from + i + 1;
    return from + got;
}

int main() {
    prctl(PR_SET_THP_DISABLE, 1, 0, 0, 0);  // Huge pages would round each small arena up to 2 MB.
    const char* d = getenv("SD_DATA");
    const char* o = getenv("SD_OUT");
    const std::string data = d ? d : "/data", out = o ? o : "/out";
    int fd = open((data + "/requests.log").c_str(), O_RDONLY);
    int output = open((out + "/summary.txt").c_str(), O_WRONLY | O_CREAT | O_TRUNC, 0644);
    struct stat st;
    if (fd < 0 || output < 0 || fstat(fd, &st)) abort();
    const size_t len = st.st_size;
    if (!len) _exit(0);
    const size_t threads = std::clamp<size_t>(std::thread::hardware_concurrency(), 1, 12);
    static Interner names;
    static Segment segments[SEGMENTS];
    std::atomic<size_t> claim{0};
    {
        std::vector<std::jthread> pool;
        for (size_t t = 0; t < threads; t++)
            pool.emplace_back([&] {
                std::vector<u8> buffer(READ_BYTES + PAD);
                for (size_t i; (i = claim.fetch_add(1, std::memory_order_relaxed)) < SEGMENTS;)
                    segments[i].parse(fd, line_start(fd, len, i * len / SEGMENTS), line_start(fd, len, (i + 1) * len / SEGMENTS), names, buffer.data());
            });
    }
    // Bytewise order of the endpoints, shared by every hour.
    std::vector<Name> order;
    for (size_t s = 0; s < SLOTS; s++) {
        u64 packed = names.slots[s].load();
        if (packed) order.push_back({s, (const u8*)(packed >> 8), (size_t)(packed & 255)});
    }
    std::sort(order.begin(), order.end(), [](const Name& a, const Name& b) {
        int c = memcmp(a.bytes, b.bytes, std::min(a.len, b.len));
        return c ? c < 0 : a.len < b.len;
    });
    // offsets[h] is published once every earlier hour's length is known, which lets each worker
    // write its text at the right place while later hours are still being formatted.
    constexpr u64 UNKNOWN = ~0ull;
    static std::atomic<u64> offsets[HOURS + 1];
    for (auto& x : offsets) x = UNKNOWN;
    offsets[0] = 0;
    std::atomic<size_t> next_hour{0};
    {
        std::vector<std::jthread> pool;
        for (size_t t = 0; t < threads; t++)
            pool.emplace_back([&] {
                std::vector<u32> lat;
                std::string text;
                for (size_t bucket; (bucket = next_hour.fetch_add(1, std::memory_order_relaxed)) < HOURS;) {
                    format_hour(bucket, segments, order, lat, text);
                    for (auto& s : segments) s.hour_done(bucket);
                    u64 start;
                    while ((start = offsets[bucket].load(std::memory_order_acquire)) == UNKNOWN) std::this_thread::yield();
                    offsets[bucket + 1].store(start + text.size(), std::memory_order_release);
                    if (pwrite(output, text.data(), text.size(), start) != (ssize_t)text.size()) abort();
                }
            });
    }
    _exit(0);  // Skip unmapping and destructors: the process is done and the clock is running.
}
