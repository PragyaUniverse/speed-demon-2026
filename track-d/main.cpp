// Track D: point-to-point shortest paths on a road-like graph.
//
// The graph is stored in both directions in a packed CSR of one u32 per edge, and each query runs a
// bidirectional Dijkstra. Weights are mostly small: an edge packs head:21 | weight:11, and the rare
// heavier edges sit in a side table. Node ids are random, so nodes are first renumbered into compact
// blobs, which puts a search's next nodes on cache lines it has already touched.
#include <fcntl.h>
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

constexpr u32 NODE_BITS = 21, WEIGHT_BITS = 32 - NODE_BITS, HEAVY = (1u << WEIGHT_BITS) - 1;
constexpr size_t PARTITIONS = 128, BLOB = 512, CHUNK = 1 << 18;
// A heap key packs distance << NODE_BITS | node, so a distance has 43 bits. The generator's only
// weights above 2^22 are single edges next to the special nodes, far below that; relaxing checks it.
constexpr u64 DIST_MASK = (1ull << (64 - NODE_BITS)) - 1;
static size_t workers;

// Parses the decimal number at `p` (ended by a space or LF); sets it, returns the byte after the end.
static inline const u8* number(const u8* p, u64& out) {
    u64 word;
    memcpy(&word, p, 8);
    // Digits are 0x30..=0x39 and the terminators have a different high nibble.
    u64 x = (word & 0xF0F0F0F0F0F0F0F0) ^ 0x3030303030303030;
    if (!x) {
        u64 v = 0;
        while (*p >= '0') v = v * 10 + (*p++ - '0');
        out = v;
        return p + 1;
    }
    size_t k = __builtin_ctzll(x) / 8;
    u64 v = (word << (64 - 8 * k)) & 0x0F0F0F0F0F0F0F0F;
    v = (v * 2561 >> 8) & 0x00FF00FF00FF00FF;
    v = (v * 6553601 >> 16) & 0x0000FFFF0000FFFF;
    out = v * 42949672960001 >> 32;
    return p + k + 1;
}

template <class F>
static void run_workers(F f) {
    std::vector<std::jthread> pool;
    for (size_t t = 0; t < workers; t++) pool.emplace_back([&f, t] { f(t); });
}

// Zeroed, and untouched pages cost nothing.
struct Array {
    u32* data = nullptr;
    size_t size = 0;
    explicit Array(size_t n = 0) : data((u32*)calloc(std::max<size_t>(n, 1), 4)), size(n) {}
    u32& operator[](size_t i) { return data[i]; }
    u32 operator[](size_t i) const { return data[i]; }
};

struct Entry {
    u32 node, edge;
};
using Buckets = std::vector<std::vector<Entry>>;

// One direction of the graph.
struct Csr {
    Array offsets, edges;
    inline const u32* begin(u32 node) const { return edges.data + offsets[node]; }
    inline const u32* end(u32 node) const { return edges.data + offsets[node + 1]; }
    u32 degree(u32 node) const { return offsets[node + 1] - offsets[node]; }
    void release() { free(offsets.data), free(edges.data); }

    // Counting sort per partition: each stays within a core's cache, and none shares any state.
    static Csr build(size_t n, u32 shift, const std::vector<Buckets>& parts) {
        std::vector<size_t> starts(PARTITIONS + 1);
        for (auto& part : parts)
            for (size_t p = 0; p < PARTITIONS; p++) starts[p + 1] += part[p].size();
        for (size_t p = 0; p < PARTITIONS; p++) starts[p + 1] += starts[p];
        Csr c{Array(n + 1), Array(starts[PARTITIONS])};
        std::atomic<size_t> claim{0};
        run_workers([&](size_t) {
            for (size_t p; (p = claim.fetch_add(1, std::memory_order_relaxed)) < PARTITIONS;) {
                // Slots first+1..=last hold counts, then starts, then ends; the end of a node is the
                // start of the next, so offsets[x] finishes as the start of x.
                size_t first = std::min(p << shift, n), last = std::min((p + 1) << shift, n);
                for (auto& part : parts)
                    for (const Entry& e : part[p]) c.offsets[e.node + 1]++;
                u32 running = starts[p];
                for (size_t x = first; x < last; x++) {
                    u32 count = c.offsets[x + 1];
                    c.offsets[x + 1] = running;
                    running += count;
                }
                for (auto& part : parts)
                    for (const Entry& e : part[p]) c.edges[c.offsets[e.node + 1]++] = e.edge;
            }
        });
        return c;
    }

    // Inverts a CSR without buckets: ids were just made local, so the shared counters a worker
    // touches are mostly on nearby lines, and no edge list is held twice in memory.
    Csr reversed() const {
        const size_t n = offsets.size - 1;
        Csr r{Array(n + 1), Array(edges.size)};
        auto each = [&](auto visit) {
            std::atomic<size_t> claim{0};
            run_workers([&](size_t) {
                constexpr size_t STEP = 1 << 12;
                for (size_t from; (from = claim.fetch_add(STEP, std::memory_order_relaxed)) < n;)
                    for (size_t u = from; u < std::min(from + STEP, n); u++)
                        for (const u32* e = begin(u); e != end(u); e++) visit((u32)u, *e);
            });
        };
        // Same three states as in `build`: counts, then starts, then ends.
        each([&](u32, u32 e) { __atomic_fetch_add(&r.offsets[(e >> WEIGHT_BITS) + 1], 1, __ATOMIC_RELAXED); });
        u32 running = 0;
        for (size_t i = 1; i <= n; i++) {
            u32 count = r.offsets[i];
            r.offsets[i] = running;
            running += count;
        }
        each([&](u32 u, u32 e) {
            u32 at = __atomic_fetch_add(&r.offsets[(e >> WEIGHT_BITS) + 1], 1, __ATOMIC_RELAXED);
            r.edges[at] = u << WEIGHT_BITS | (e & HEAVY);
        });
        return r;
    }
};

// Renumbers the nodes so that neighbours share cache lines: workers grow compact blobs by BFS from
// unclaimed seeds and each blob gets consecutive ids.
static Csr relabel(size_t n, Csr old, std::vector<u32>& rank) {
    rank.assign(n, 0);  // First 0 or 1 for "claimed", then each node's new id.
    u32* claimed = rank.data();
    std::atomic<size_t> seed{0};
    std::vector<std::vector<u32>> orders(workers);
    std::vector<size_t> edge_counts(workers);
    run_workers([&](size_t me) {
        auto& order = orders[me];
        order.reserve(n / workers * 5 / 4);
        size_t edge_count = 0;
        auto claim = [&](size_t x) {
            u32 zero = 0;
            return __atomic_load_n(&claimed[x], __ATOMIC_RELAXED) == 0 && __atomic_compare_exchange_n(&claimed[x], &zero, 1, false, __ATOMIC_RELAXED, __ATOMIC_RELAXED);
        };
        for (size_t from; (from = seed.fetch_add(1024, std::memory_order_relaxed)) < n;)
            for (size_t x = from; x < std::min(from + 1024, n); x++) {
                if (!claim(x)) continue;
                size_t blob = order.size();
                order.push_back(x);
                for (size_t at = blob; at < order.size(); at++) {
                    u32 node = order[at];
                    edge_count += old.degree(node);
                    for (const u32* e = old.begin(node); e != old.end(node); e++) {
                        u32 v = *e >> WEIGHT_BITS;
                        if (order.size() - blob < BLOB && claim(v)) order.push_back(v);
                    }
                }
            }
        edge_counts[me] = edge_count;
    });
    size_t total = 0;
    for (size_t c : edge_counts) total += c;
    Csr c{Array(n + 1), Array(total)};
    c.offsets[n] = total;
    std::vector<size_t> node_base(workers), edge_base(workers);
    for (size_t t = 1; t < workers; t++) {
        node_base[t] = node_base[t - 1] + orders[t - 1].size();
        edge_base[t] = edge_base[t - 1] + edge_counts[t - 1];
    }
    run_workers([&](size_t me) {
        for (size_t i = 0; i < orders[me].size(); i++) rank[orders[me][i]] = node_base[me] + i;
    });
    run_workers([&](size_t me) {
        size_t pos = edge_base[me];
        for (size_t i = 0; i < orders[me].size(); i++) {
            u32 x = orders[me][i];
            c.offsets[node_base[me] + i] = pos;
            for (const u32* e = old.begin(x); e != old.end(x); e++) c.edges[pos++] = rank[*e >> WEIGHT_BITS] << WEIGHT_BITS | (*e & HEAVY);
        }
    });
    old.release();
    return c;
}

struct Graph {
    Csr forward, backward;
    std::vector<std::pair<u64, u32>> heavy;  // (tail << NODE_BITS | head, lightest heavy weight), sorted.
    u64 heavy_weight(u32 tail, u32 head) const {
        u64 key = (u64)tail << NODE_BITS | head;
        auto it = std::lower_bound(heavy.begin(), heavy.end(), key, [](auto& e, u64 k) { return e.first < k; });
        return it->second;
    }
};

static Graph parse_graph(int fd, std::vector<u32>& rank) {
    struct stat st;
    if (fstat(fd, &st)) abort();
    const size_t len = st.st_size;
    u8 header[64];
    if (pread(fd, header, sizeof header, 0) <= 0) abort();
    u64 n_, m;
    const u8* p = number(number(header, n_), m);
    const size_t n = n_, start = p - header;
    if (n >= (1u << NODE_BITS) - 1) abort();
    u32 bits = n > 2 ? 64 - __builtin_clzll(n - 1) : 1;
    const u32 shift = bits > 7 ? bits - 7 : 0;
    auto line_start = [&](size_t at) {
        if (at <= start) return start;
        at = std::min(at, len);
        u8 probe[64];
        ssize_t got = pread(fd, probe, sizeof probe, at);
        for (ssize_t i = 0; i < got; i++)
            if (probe[i] == '\n') return at + i + 1;
        return at + (size_t)std::max<ssize_t>(got, 0);
    };
    // Workers claim newline-aligned chunks, read each into a buffer that stays in this core's L2,
    // and spill forward edges into per-partition buckets.
    std::atomic<size_t> claim{0};
    std::mutex heavy_lock;
    std::vector<std::pair<u64, u32>> heavy;
    std::vector<Buckets> parts(workers);
    run_workers([&](size_t me) {
        Buckets& buckets = parts[me];
        buckets.resize(PARTITIONS);
        // Sized once: growing 1,536 vectors would leave holes worth a third of the data.
        for (auto& b : buckets) b.reserve(m / PARTITIONS / workers * 5 / 4);
        std::vector<std::pair<u64, u32>> local;
        std::vector<u8> buffer(CHUNK + 256);
        for (;;) {
            size_t i = claim.fetch_add(1, std::memory_order_relaxed), from = start + i * CHUNK;
            if (from >= len) break;
            size_t first = line_start(from), last = line_start(from + CHUNK);
            if (last > first && pread(fd, buffer.data(), last - first, first) != (ssize_t)(last - first)) abort();
            memset(buffer.data() + (last - first), 0, 16);
            const u8* q = buffer.data();
            const u8* stop = q + (last - first);
            while (q < stop) {
                u64 u, v, w;
                q = number(number(number(q, u), v), w);
                if (w >= HEAVY) local.push_back({u << NODE_BITS | v, (u32)w});
                buckets[u >> shift].push_back({(u32)u, (u32)v << WEIGHT_BITS | std::min<u32>(w, HEAVY)});
            }
        }
        std::lock_guard<std::mutex> guard(heavy_lock);
        heavy.insert(heavy.end(), local.begin(), local.end());
    });
    Csr old = Csr::build(n, shift, parts);
    std::vector<Buckets>().swap(parts);
    Graph g;
    g.forward = relabel(n, old, rank);
    for (auto& e : heavy) e.first = (u64)rank[e.first >> NODE_BITS] << NODE_BITS | rank[e.first & ((1ull << NODE_BITS) - 1)];
    std::sort(heavy.begin(), heavy.end());
    heavy.erase(std::unique(heavy.begin(), heavy.end(), [](auto& a, auto& b) { return a.first == b.first; }), heavy.end());
    g.heavy = std::move(heavy);
    g.backward = g.forward.reversed();
    return g;
}

// 4-ary min-heap of packed (distance, node) keys: comparing one u64 is all a sift needs.
struct Heap {
    std::vector<u64> a;
    inline void push(u64 key) {
        size_t i = a.size();
        a.push_back(key);
        while (i > 0) {
            size_t parent = (i - 1) / 4;
            if (a[parent] <= key) break;
            a[i] = a[parent];
            i = parent;
        }
        a[i] = key;
    }
    inline u64 pop() {
        u64 top = a[0], last = a.back();
        a.pop_back();
        size_t n = a.size();
        if (!n) return top;
        size_t i = 0;
        for (;;) {
            size_t first = 4 * i + 1;
            if (first >= n) break;
            size_t best = first, end = std::min(first + 4, n);
            for (size_t c = first + 1; c < end; c++)
                if (a[c] < a[best]) best = c;
            if (a[best] >= last) break;
            a[i] = a[best];
            i = best;
        }
        a[i] = last;
        return top;
    }
};

enum class Outcome { Found, NoPath, TooFar };
constexpr u32 PAGE_BITS = 5, PAGE = 1u << PAGE_BITS, PAGE_INDEX_BITS = NODE_BITS - PAGE_BITS;

// One thread's search state: distances live in small pages allocated on demand behind a directory
// with one entry per PAGE consecutive ids. Relabelling made neighbours consecutive, so pages fill
// up, and unlike a hash table a lookup has no probing branch to mispredict. Nearly every search fits
// in u32, which halves its memory; the few that meet a huge weight give up and rerun with u64.
template <class D, u64 LIMIT>
struct Search {
    static constexpr D UNSET = (D)LIMIT;
    struct Slot {
        D d[2];
    };
    struct Page {
        Slot s[PAGE];
    };
    std::vector<u32> directory;  // epoch << PAGE_INDEX_BITS | page index; stale epochs read as absent.
    u32 epoch = 0;
    std::vector<Page> pages;
    size_t used = 0;
    Heap heaps[2];
    explicit Search(size_t n) : directory((n >> PAGE_BITS) + 1) {}

    inline Slot& entry(u32 node) {
        u32& dir = directory[node >> PAGE_BITS];
        if (dir >> PAGE_INDEX_BITS != epoch) {
            if (used == pages.size()) pages.emplace_back();
            Page& page = pages[used];
            for (Slot& s : page.s) s.d[0] = s.d[1] = UNSET;
            dir = epoch << PAGE_INDEX_BITS | used++;
        }
        return pages[dir & ((1u << PAGE_INDEX_BITS) - 1)].s[node & (PAGE - 1)];
    }

    Outcome run(const Graph& g, u32 s, u32 t, u64& answer) {
        if (s == t) {
            answer = 0;
            return Outcome::Found;
        }
        // Answering these early skips what would otherwise be a search of the whole main body.
        if (!g.forward.degree(s) || !g.backward.degree(t)) return Outcome::NoPath;
        if (++epoch >> (32 - PAGE_INDEX_BITS)) {
            std::fill(directory.begin(), directory.end(), 0);
            epoch = 1;
        }
        used = 0;
        heaps[0].a.clear(), heaps[1].a.clear();
        entry(s).d[0] = 0;
        entry(t).d[1] = 0;
        heaps[0].push(s), heaps[1].push(t);
        u64 best = ~0ull;
        while (!heaps[0].a.empty() && !heaps[1].a.empty()) {
            u64 top0 = heaps[0].a[0] >> NODE_BITS, top1 = heaps[1].a[0] >> NODE_BITS;
            if (top0 + top1 >= best) break;
            int side = heaps[1].a.size() < heaps[0].a.size();
            u64 other_top = side ? top0 : top1, key = heaps[side].pop(), d = key >> NODE_BITS;
            u32 node = key & ((1u << NODE_BITS) - 1);
            if (d != entry(node).d[side]) continue;  // Stale: reached more cheaply after this push.
            const Csr& csr = side ? g.backward : g.forward;
            for (const u32* e = csr.begin(node), *stop = csr.end(node); e != stop; e++) {
                u32 head = *e >> WEIGHT_BITS;
                u64 weight = *e & HEAVY;
                if (weight == HEAVY) weight = side ? g.heavy_weight(head, node) : g.heavy_weight(node, head);
                u64 nd = d + weight;
                if (nd >= LIMIT) return Outcome::TooFar;
                Slot& slot = entry(head);
                if (nd < slot.d[side]) {
                    slot.d[side] = nd;
                    D other = slot.d[1 - side];
                    if (other != UNSET) best = std::min(best, nd + other);
                    // Any path through this node also needs at least the other side's frontier.
                    if (nd + other_top < best) heaps[side].push(nd << NODE_BITS | head);
                }
            }
        }
        answer = best;
        return best == ~0ull ? Outcome::NoPath : Outcome::Found;
    }
};

int main() {
    prctl(PR_SET_THP_DISABLE, 1, 0, 0, 0);  // Huge pages would round each small arena up to 2 MB.
    workers = std::clamp<size_t>(std::thread::hardware_concurrency(), 1, 12);
    const char* d = getenv("SD_DATA");
    const char* o = getenv("SD_OUT");
    const std::string data = d ? d : "/data", out = o ? o : "/out";
    int fd = open((data + "/graph.txt").c_str(), O_RDONLY);
    if (fd < 0) abort();
    std::vector<u32> rank;
    Graph graph = parse_graph(fd, rank);
    std::vector<std::pair<u32, u32>> queries;
    {
        int qf = open((data + "/queries.txt").c_str(), O_RDONLY);
        struct stat st;
        if (qf < 0 || fstat(qf, &st)) abort();
        std::vector<u8> text(st.st_size + 16);
        if (read(qf, text.data(), st.st_size) != st.st_size) abort();
        for (const u8* p = text.data(), *stop = p + st.st_size; p < stop;) {
            u64 s, t;
            p = number(number(p, s), t);
            queries.push_back({rank[s], rank[t]});
        }
    }
    constexpr u64 NO_PATH = ~0ull;
    std::vector<u64> answers(queries.size(), NO_PATH);
    std::atomic<size_t> claim{0};
    run_workers([&](size_t) {
        Search<u32, 0xFFFFFFFFu> search(rank.size());
        for (size_t i; (i = claim.fetch_add(1, std::memory_order_relaxed)) < queries.size();) {
            u64 a;
            Outcome r = search.run(graph, queries[i].first, queries[i].second, a);
            if (r == Outcome::TooFar) {
                Search<u64, DIST_MASK> wide(rank.size());
                r = wide.run(graph, queries[i].first, queries[i].second, a);
            }
            if (r == Outcome::Found) answers[i] = a;
        }
    });
    std::string lines;
    for (u64 a : answers) lines += a == NO_PATH ? "-1\n" : std::to_string(a) + "\n";
    int output = open((out + "/distances.txt").c_str(), O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (output < 0 || write(output, lines.data(), lines.size()) != (ssize_t)lines.size()) abort();
    _exit(0);  // Skip unmapping and destructors: the process is done and the clock is running.
}
