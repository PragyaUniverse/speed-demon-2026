//! Track D: point-to-point shortest paths on a road-like graph.
//!
//! The graph is stored in both directions in a packed CSR of one u32 per edge, and each query runs
//! a bidirectional Dijkstra that visits about half of what a forward search would. Weights are
//! mostly small: an edge packs head:21 | weight:11, and the rare heavier edges sit in a side table.
//! Search state lives in a small per-thread hash table, not a node-indexed array, so the many short
//! searches touch little memory and 12 threads do not need 12 arrays of 1.7M distances.
use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::*};
use std::sync::Mutex;
use std::{env, ptr, slice, thread};

const NODE_BITS: u32 = 21;
const WEIGHT_BITS: u32 = 32 - NODE_BITS;
const HEAVY: u32 = (1 << WEIGHT_BITS) - 1; // Packed weight meaning "look it up in the side table".
const PARTITIONS: usize = 128;

// Search keys and table slots pack a distance into 43 bits. The generator's only weights above
// 2^22 are single edges next to the special nodes, so a path stays far below that; relaxing a
// heavy edge checks it anyway.
const DIST_BITS: u32 = 64 - NODE_BITS;
const DIST_MASK: u64 = (1 << DIST_BITS) - 1;
const UNSET: u64 = DIST_MASK;

const PROT_READ: i32 = 1;
const MAP_PRIVATE: i32 = 2;
const MAP_FIXED: i32 = 0x10;
#[cfg(target_os = "linux")]
const MAP_ANON: i32 = 0x20;
#[cfg(not(target_os = "linux"))]
const MAP_ANON: i32 = 0x1000;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
}

/// Maps a file with one zeroed page behind it, so word reads may run past the end of the data.
fn map_file(path: String) -> &'static [u8] {
    let file = File::open(path).unwrap();
    let len = file.metadata().unwrap().len() as usize;
    unsafe {
        let base = mmap(ptr::null_mut(), len + 4096, PROT_READ, MAP_PRIVATE | MAP_ANON, -1, 0);
        assert_eq!(mmap(base, len, PROT_READ, MAP_PRIVATE | MAP_FIXED, file.as_raw_fd(), 0), base);
        slice::from_raw_parts(base, len)
    }
}

/// Parses the decimal number at `p` (ended by a space or LF); returns it and the byte after the end.
#[inline(always)]
unsafe fn number(p: *const u8) -> (u64, *const u8) {
    let word = (p as *const u64).read_unaligned();
    // Digits are 0x30..=0x39 and the terminators have a different high nibble.
    let k = (((word & 0xF0F0F0F0F0F0F0F0) ^ 0x3030303030303030).trailing_zeros() / 8) as usize;
    if k == 8 {
        let (mut v, mut q) = (0, p);
        while *q >= b'0' {
            v = v * 10 + (*q - b'0') as u64;
            q = q.add(1);
        }
        return (v, q.add(1));
    }
    let v = (word << (64 - 8 * k)) & 0x0F0F0F0F0F0F0F0F;
    let v = (v.wrapping_mul(2561) >> 8) & 0x00FF00FF00FF00FF;
    let v = (v.wrapping_mul(6553601) >> 16) & 0x0000FFFF0000FFFF;
    (v.wrapping_mul(42949672960001) >> 32, p.add(k + 1))
}

#[inline(always)]
fn pack(node: u32, weight: u32) -> u32 {
    node << WEIGHT_BITS | weight.min(HEAVY)
}

/// Edges on their way into a CSR, bucketed by key node so that each partition builds on its own.
type Buckets = Vec<Vec<(u32, u32)>>;

/// One direction of the graph.
struct Csr {
    offsets: Vec<u32>,
    edges: Vec<u32>,
}

impl Csr {
    /// Counting sort per partition: each stays within a core's cache, and none shares any state.
    fn build(n: usize, shift: u32, parts: Vec<Buckets>) -> Csr {
        let mut starts = vec![0usize; PARTITIONS + 1];
        for part in &parts {
            for (p, bucket) in part.iter().enumerate() {
                starts[p + 1] += bucket.len();
            }
        }
        for p in 0..PARTITIONS {
            starts[p + 1] += starts[p];
        }
        let mut offsets = vec![0u32; n + 1];
        let mut edges = Vec::<u32>::with_capacity(starts[PARTITIONS]);
        unsafe { edges.set_len(starts[PARTITIONS]) };
        let (off, out) = (offsets.as_mut_ptr() as usize, edges.as_mut_ptr() as usize);
        let claim = AtomicUsize::new(0);
        thread::scope(|scope| {
            for _ in 0..workers() {
                scope.spawn(|| unsafe {
                    let (off, out) = (off as *mut u32, out as *mut u32);
                    loop {
                        let p = claim.fetch_add(1, Relaxed);
                        if p >= PARTITIONS {
                            return;
                        }
                        // Slots first+1..=last hold counts, then starts, then ends; the end of a node
                        // is the start of the next, so offsets[x] finishes as the start of x.
                        let (first, last) = ((p << shift).min(n), ((p + 1) << shift).min(n));
                        for part in &parts {
                            for &(node, _) in &part[p] {
                                *off.add(node as usize + 1) += 1;
                            }
                        }
                        let mut running = starts[p] as u32;
                        for x in first..last {
                            let count = *off.add(x + 1);
                            *off.add(x + 1) = running;
                            running += count;
                        }
                        for part in &parts {
                            for &(node, edge) in &part[p] {
                                let cursor = off.add(node as usize + 1);
                                *out.add(*cursor as usize) = edge;
                                *cursor += 1;
                            }
                        }
                    }
                });
            }
        });
        Csr { offsets, edges }
    }

    #[inline(always)]
    fn adjacent(&self, node: u32) -> &[u32] {
        unsafe {
            let from = *self.offsets.get_unchecked(node as usize) as usize;
            let to = *self.offsets.get_unchecked(node as usize + 1) as usize;
            self.edges.get_unchecked(from..to)
        }
    }

    fn degree(&self, node: u32) -> u32 {
        self.offsets[node as usize + 1] - self.offsets[node as usize]
    }
}

fn workers() -> usize {
    thread::available_parallelism().map_or(12, |n| n.get()).min(12)
}

struct Graph {
    forward: Csr,
    backward: Csr,
    /// (tail << NODE_BITS | head, lightest heavy weight), sorted.
    heavy: Vec<(u64, u32)>,
}

impl Graph {
    fn parse(input: &'static [u8]) -> (usize, Graph) {
        let mut p = input.as_ptr();
        let (n, m);
        unsafe {
            (n, p) = number(p);
            (m, p) = number(p);
        }
        let (n, _) = (n as usize, m);
        assert!(n < (1 << NODE_BITS) - 1);
        let shift = ((n.max(2) - 1).ilog2() + 1).saturating_sub(PARTITIONS.ilog2());
        let start = p as usize - input.as_ptr() as usize;

        // Workers claim newline-aligned chunks and spill forward edges into per-partition buckets.
        const CHUNK: usize = 1 << 20;
        let claim = AtomicUsize::new(0);
        let line_start = |at: usize| {
            if at <= start {
                return start;
            }
            let at = at.min(input.len());
            at + input[at..].iter().position(|&b| b == b'\n').map_or(input.len() - at, |i| i + 1)
        };
        let heavy = Mutex::new(Vec::new());
        let parts: Vec<Buckets> = thread::scope(|scope| {
            let handles: Vec<_> = (0..workers())
                .map(|_| {
                    scope.spawn(|| unsafe {
                        let mut buckets: Buckets = vec![Vec::new(); PARTITIONS];
                        let mut local_heavy = Vec::new();
                        loop {
                            let i = claim.fetch_add(1, Relaxed);
                            let (from, to) = (start + i * CHUNK, start + (i + 1) * CHUNK);
                            if from >= input.len() {
                                heavy.lock().unwrap().extend_from_slice(&local_heavy);
                                return buckets;
                            }
                            let mut q = input.as_ptr().add(line_start(from));
                            let stop = input.as_ptr().add(line_start(to));
                            while q < stop {
                                let (u, r) = number(q);
                                let (v, r) = number(r);
                                let (w, r) = number(r);
                                q = r;
                                let (u, v, w) = (u as u32, v as u32, w as u32);
                                if w >= HEAVY {
                                    local_heavy.push(((u as u64) << NODE_BITS | v as u64, w));
                                }
                                buckets.get_unchecked_mut((u >> shift) as usize).push((u, pack(v, w)));
                            }
                        }
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let mut heavy = heavy.into_inner().unwrap();
        heavy.sort_unstable();
        heavy.dedup_by_key(|e| e.0); // Sorted by weight within a key: the lightest stays.

        let forward = Csr::build(n, shift, parts);
        let backward = Csr::build(n, shift, Self::reversed(n, shift, &forward));
        (n, Graph { forward, backward, heavy })
    }

    /// Buckets every edge by its head, ready to build the backward CSR.
    fn reversed(n: usize, shift: u32, forward: &Csr) -> Vec<Buckets> {
        let claim = AtomicUsize::new(0);
        const STEP: usize = 1 << 14;
        thread::scope(|scope| {
            let handles: Vec<_> = (0..workers())
                .map(|_| {
                    scope.spawn(|| {
                        let mut buckets: Buckets = vec![Vec::new(); PARTITIONS];
                        loop {
                            let from = claim.fetch_add(STEP, Relaxed);
                            if from >= n {
                                return buckets;
                            }
                            for u in from..(from + STEP).min(n) {
                                for &e in forward.adjacent(u as u32) {
                                    let v = e >> WEIGHT_BITS;
                                    let edge = (u as u32) << WEIGHT_BITS | (e & HEAVY);
                                    unsafe { buckets.get_unchecked_mut((v >> shift) as usize).push((v, edge)) };
                                }
                            }
                        }
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        })
    }

    #[inline(never)]
    fn heavy_weight(&self, tail: u32, head: u32) -> u64 {
        let key = (tail as u64) << NODE_BITS | head as u64;
        self.heavy[self.heavy.binary_search_by_key(&key, |e| e.0).unwrap()].1 as u64
    }
}

/// Binary min-heap of packed (distance, node) keys: comparing one u64 is all a sift needs.
#[derive(Default)]
struct Heap(Vec<u64>);

impl Heap {
    #[inline(always)]
    fn push(&mut self, key: u64) {
        let mut i = self.0.len();
        self.0.push(key);
        unsafe {
            let a = self.0.as_mut_ptr();
            while i > 0 {
                let parent = (i - 1) / 4;
                if *a.add(parent) <= key {
                    break;
                }
                *a.add(i) = *a.add(parent);
                i = parent;
            }
            *a.add(i) = key;
        }
    }

    #[inline(always)]
    fn pop(&mut self) -> u64 {
        let top = self.0[0];
        let last = self.0.pop().unwrap();
        let n = self.0.len();
        if n == 0 {
            return top;
        }
        unsafe {
            let a = self.0.as_mut_ptr();
            let mut i = 0;
            loop {
                let first = 4 * i + 1;
                if first >= n {
                    break;
                }
                // Four children share a cache line most of the time; the smallest moves up.
                let mut best = first;
                let end = (first + 4).min(n);
                let mut c = first + 1;
                while c < end {
                    if *a.add(c) < *a.add(best) {
                        best = c;
                    }
                    c += 1;
                }
                if *a.add(best) >= last {
                    break;
                }
                *a.add(i) = *a.add(best);
                i = best;
            }
            *a.add(i) = last;
        }
        top
    }
}

/// One thread's search state. A slot packs (node + 1) << DIST_BITS | forward distance, then the
/// backward distance: visiting a node also tells whether the other side has reached it.
struct Search {
    slots: Vec<[u64; 2]>,
    bits: u32,
    used: usize,
    heaps: [Heap; 2],
}

const FIRST_BITS: u32 = 10;

impl Search {
    fn new() -> Self {
        Search { slots: vec![[0; 2]; 1 << FIRST_BITS], bits: FIRST_BITS, used: 0, heaps: Default::default() }
    }

    /// Slot index of a node, claiming a free slot with both distances unset.
    #[inline(always)]
    fn slot(&mut self, node: u32) -> usize {
        let tag = (node as u64 + 1) << DIST_BITS;
        let mask = (1usize << self.bits) - 1;
        let mut i = ((node as u64).wrapping_mul(0x9E3779B97F4A7C15) >> (64 - self.bits)) as usize;
        loop {
            let s = unsafe { self.slots.get_unchecked_mut(i) };
            if s[0] & !DIST_MASK == tag {
                return i;
            }
            if s[0] == 0 {
                *s = [tag | UNSET, UNSET];
                self.used += 1;
                return i;
            }
            i = (i + 1) & mask;
        }
    }

    /// Doubles the table in place once it is half full. Heap keys hold nodes, not slots, so only
    /// the table itself changes.
    #[cold]
    fn grow(&mut self) {
        let old: Vec<[u64; 2]> = self.slots[..1 << self.bits].to_vec();
        self.bits += 1;
        if self.slots.len() < 1 << self.bits {
            self.slots.resize(1 << self.bits, [0; 2]);
        }
        self.slots[..1 << self.bits].fill([0; 2]);
        self.used = 0;
        for entry in old.iter().filter(|e| e[0] != 0) {
            let at = self.slot(((entry[0] >> DIST_BITS) - 1) as u32);
            self.slots[at] = *entry;
        }
    }

    #[inline(always)]
    fn distance(&self, at: usize, side: usize) -> u64 {
        let s = unsafe { self.slots.get_unchecked(at) };
        if side == 0 { s[0] & DIST_MASK } else { s[1] }
    }

    #[inline(always)]
    fn set_distance(&mut self, at: usize, side: usize, d: u64) {
        let s = unsafe { self.slots.get_unchecked_mut(at) };
        if side == 0 {
            s[0] = (s[0] & !DIST_MASK) | d;
        } else {
            s[1] = d;
        }
    }

    /// Returns the s-t distance, or None if there is no path.
    fn run(&mut self, g: &Graph, s: u32, t: u32) -> Option<u64> {
        if s == t {
            return Some(0);
        }
        // Answering these early skips what would otherwise be a search of the whole main body.
        if g.forward.degree(s) == 0 || g.backward.degree(t) == 0 {
            return None;
        }
        self.slots[..1 << self.bits].fill([0; 2]);
        self.bits = FIRST_BITS;
        self.used = 0;
        self.heaps[0].0.clear();
        self.heaps[1].0.clear();
        let at = self.slot(s);
        self.set_distance(at, 0, 0);
        let at = self.slot(t);
        self.set_distance(at, 1, 0);
        self.heaps[0].push(s as u64);
        self.heaps[1].push(t as u64);
        let mut best = u64::MAX;
        const NODE_MASK: u64 = (1 << NODE_BITS) - 1;
        while let (Some(&top0), Some(&top1)) = (self.heaps[0].0.first(), self.heaps[1].0.first()) {
            if (top0 >> NODE_BITS) + (top1 >> NODE_BITS) >= best {
                break;
            }
            let side = (self.heaps[1].0.len() < self.heaps[0].0.len()) as usize;
            let key = self.heaps[side].pop();
            let (d, node) = (key >> NODE_BITS, (key & NODE_MASK) as u32);
            let at = self.slot(node);
            if d != self.distance(at, side) {
                continue; // A stale entry: the node was reached more cheaply after this push.
            }
            let csr = if side == 0 { &g.forward } else { &g.backward };
            for &edge in csr.adjacent(node) {
                let head = edge >> WEIGHT_BITS;
                let mut weight = (edge & HEAVY) as u64;
                if weight == HEAVY as u64 {
                    weight = if side == 0 { g.heavy_weight(node, head) } else { g.heavy_weight(head, node) };
                    assert!(d + weight < UNSET);
                }
                let nd = d + weight;
                let at = self.slot(head);
                if nd < self.distance(at, side) {
                    self.set_distance(at, side, nd);
                    self.heaps[side].push(nd << NODE_BITS | head as u64);
                    let other = self.distance(at, 1 - side);
                    if other != UNSET {
                        best = best.min(nd + other);
                    }
                }
                if self.used * 2 > 1 << self.bits {
                    self.grow();
                }
            }
        }
        (best != u64::MAX).then_some(best)
    }
}

fn main() {
    let data = env::var("SD_DATA").unwrap_or("/data".into());
    let out = env::var("SD_OUT").unwrap_or("/out".into());
    let (n, graph) = Graph::parse(map_file(format!("{data}/graph.txt")));
    let _ = n;
    let text = std::fs::read(format!("{data}/queries.txt")).unwrap();
    let mut queries = Vec::new();
    let mut fields = text.split(|&b| b == b' ' || b == b'\n').filter(|f| !f.is_empty()).map(|f| {
        f.iter().fold(0u32, |v, &d| v * 10 + (d - b'0') as u32)
    });
    while let (Some(s), Some(t)) = (fields.next(), fields.next()) {
        queries.push((s, t));
    }

    const NO_PATH: u64 = u64::MAX;
    let answers: Vec<AtomicU64> = queries.iter().map(|_| AtomicU64::new(NO_PATH)).collect();
    let claim = AtomicUsize::new(0);
    thread::scope(|scope| {
        for _ in 0..workers() {
            scope.spawn(|| {
                let mut search = Search::new();
                loop {
                    let i = claim.fetch_add(1, Relaxed);
                    let Some(&(s, t)) = queries.get(i) else { return };
                    if let Some(d) = search.run(&graph, s, t) {
                        answers[i].store(d, Relaxed);
                    }
                }
            });
        }
    });

    let mut lines = String::new();
    for a in &answers {
        match a.load(Relaxed) {
            NO_PATH => lines.push_str("-1\n"),
            d => lines.push_str(&format!("{d}\n")),
        }
    }
    File::create(format!("{out}/distances.txt")).unwrap().write_all(lines.as_bytes()).unwrap();
    // Skip unmapping and destructors: the process is done and the clock is running.
    std::process::exit(0);
}
