//! Track C: hostile CSV to per-country aggregates.
//!
//! The parser walks one field at a time with branch-free word tricks. Workers can start anywhere
//! because a record start is recognisable without tracking quote state: the generator's text is
//! letters and punctuation only (spec 2.2), so an LF followed by a digit or a BOM is always a real
//! record boundary, and an LF inside a quoted note never is.
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering::*};
use std::sync::Mutex;
use std::{env, ptr, slice, thread};

const SEGMENTS: usize = 64;
const PAGE: usize = 1 << 14; // A multiple of every page size this runs on.
const UNMATCHED: usize = 0; // Group index; countries get 1, 2, ... in order of first appearance.

// Names are interned in one shared table of 2^18 slots; the pool holds 100,000. A slot packs the
// high hash bits with id + 1, and the slot index holds the low bits, so a match means all 64 hash
// bits agree (about 1e-10 chance of any false match in a run).
const ID_BITS: u32 = 18;
const ID_MASK: u64 = (1 << ID_BITS) - 1;

const PROT_READ: i32 = 1;
const PROT_WRITE: i32 = 2;
const MAP_PRIVATE: i32 = 2;
const MAP_FIXED: i32 = 0x10;
#[cfg(target_os = "linux")]
const MAP_ANON: i32 = 0x20;
#[cfg(not(target_os = "linux"))]
const MAP_ANON: i32 = 0x1000;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
    #[cfg(target_os = "linux")]
    fn prctl(option: i32, a: u64, b: u64, c: u64, d: u64) -> i32;
}

/// Maps a file with one zeroed page behind it, so word reads may run past the end of the data.
fn map_file(path: String) -> &'static [u8] {
    let file = File::open(path).unwrap();
    let len = file.metadata().unwrap().len() as usize;
    unsafe {
        let base = mmap(ptr::null_mut(), len + 4096, PROT_READ, MAP_PRIVATE | MAP_ANON, -1, 0);
        if len > 0 {
            assert_eq!(mmap(base, len, PROT_READ, MAP_PRIVATE | MAP_FIXED, file.as_raw_fd(), 0), base);
        }
        slice::from_raw_parts(base, len)
    }
}

/// Zeroed atomics that cost memory only where they are touched.
fn lazy_table<T>(n: usize) -> &'static [T] {
    let bytes = n * std::mem::size_of::<T>();
    unsafe {
        let p = mmap(ptr::null_mut(), bytes, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, -1, 0);
        slice::from_raw_parts(p as *const T, n)
    }
}

#[inline(always)]
unsafe fn load(p: *const u8) -> u64 {
    (p as *const u64).read_unaligned()
}

const LOW: u64 = 0x0101010101010101;
const HIGH: u64 = 0x8080808080808080;

/// Byte index of the first zero byte of the little-endian word, 8 if there is none.
#[inline(always)]
fn first_zero(word: u64) -> usize {
    (((word.wrapping_sub(LOW) & !word & HIGH).trailing_zeros()) / 8) as usize
}

/// Byte index of the first byte equal to `b`, 8 if there is none.
#[inline(always)]
fn first_byte(word: u64, b: u8) -> usize {
    first_zero(word ^ (b as u64 * LOW))
}

/// Combines up to 8 ASCII digits sitting in the high bytes of `word`, one multiply per level.
#[inline(always)]
fn digits8(word: u64) -> u64 {
    let v = word & 0x0F0F0F0F0F0F0F0F;
    let v = (v.wrapping_mul(2561) >> 8) & 0x00FF00FF00FF00FF;
    let v = (v.wrapping_mul(6553601) >> 16) & 0x0000FFFF0000FFFF;
    v.wrapping_mul(42949672960001) >> 32
}

/// Parses a decimal field of 1 to 8 digits ending in a comma; returns it and the next field.
#[inline(always)]
unsafe fn number_field(p: *const u8) -> (u64, *const u8) {
    let word = load(p);
    let k = first_byte(word, b',');
    (digits8(word << (64 - 8 * k)), p.add(k + 1))
}

/// Distance to the next `b`, which is known to exist.
#[cfg(target_feature = "avx2")]
#[inline(always)]
unsafe fn until(p: *const u8, b: u8) -> usize {
    use std::arch::x86_64::*;
    let needle = _mm256_set1_epi8(b as i8);
    let mut n = 0;
    loop {
        let v = _mm256_loadu_si256(p.add(n) as *const __m256i);
        let m = _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, needle)) as u32;
        if m != 0 {
            return n + m.trailing_zeros() as usize;
        }
        n += 32;
    }
}

#[cfg(not(target_feature = "avx2"))]
#[inline(always)]
unsafe fn until(p: *const u8, b: u8) -> usize {
    let mut n = 0;
    loop {
        let k = first_byte(load(p.add(n)), b);
        if k < 8 {
            return n + k;
        }
        n += 8;
    }
}

/// A text field: its content bytes as written, its unescaped length, and where the next field is.
/// Raw content is safe to compare: `""` is the only escape and unquoted text cannot contain a quote.
#[inline(always)]
unsafe fn text_field(p: *const u8) -> (*const u8, usize, usize, *const u8) {
    if *p != b'"' {
        let n = until(p, b',');
        return (p, n, n, p.add(n + 1));
    }
    let start = p.add(1);
    let (mut q, mut escapes) = (start, 0);
    loop {
        q = q.add(until(q, b'"'));
        if *q.add(1) != b'"' {
            break;
        }
        q = q.add(2);
        escapes += 1;
    }
    let raw = q as usize - start as usize;
    (start, raw, raw - escapes, q.add(2))
}

/// Multiply-mix over all bytes of the name, with a masked load so that nothing past it counts.
#[inline(always)]
unsafe fn hash_name(p: *const u8, len: usize) -> u64 {
    const K: u64 = 0x9E3779B97F4A7C15;
    let mut h = (len as u64).wrapping_mul(K);
    let mut i = 0;
    while i + 8 < len {
        h = (h ^ load(p.add(i))).wrapping_mul(K).rotate_left(29);
        i += 8;
    }
    if len > 0 {
        let rest = len - i; // 1 to 8 bytes.
        h = (h ^ (load(p.add(i)) & (u64::MAX >> (64 - 8 * rest)))).wrapping_mul(K);
    }
    h ^ (h >> 32)
}

#[derive(Clone, Copy)]
struct Group {
    count: u64,
    id_sum: u64,
    amount_sum: i64,
    amount_min: i64,
    amount_max: i64,
    name_bytes: u64,
    note_bytes: u64,
}

const EMPTY: Group = Group {
    count: 0,
    id_sum: 0,
    amount_sum: 0,
    amount_min: i64::MAX,
    amount_max: i64::MIN,
    name_bytes: 0,
    note_bytes: 0,
};

/// State shared by the workers while they parse.
struct Shared {
    /// region_id -> group index; one extra zero at the end catches every larger region_id.
    region_group: Vec<u8>,
    names: &'static [AtomicU64],
    next_name: AtomicU32,
    /// Bit matrix: one row of `stride` words per 64 name ids, so ids stay dense in memory. Each
    /// cache line (the eight groups sharing it) has one owning worker, the only one to touch it:
    /// the matrix stays in that core's L2 instead of bouncing between cores on every record.
    seen: &'static [AtomicU64],
    stride: usize,
    /// Each worker's incoming (name id, group) pairs for the lines it owns.
    inboxes: Vec<Mutex<Vec<u32>>>,
    finished: AtomicUsize,
}

impl Shared {
    /// Dense id of a name, claiming a slot and an id if it is new.
    #[inline(always)]
    fn name_id(&self, hash: u64) -> usize {
        let tag = hash & !ID_MASK;
        let mut slot = (hash & ID_MASK) as usize;
        loop {
            let entry = unsafe { self.names.get_unchecked(slot) }.load(Relaxed);
            if entry & !ID_MASK == tag && entry != 0 {
                return (entry & ID_MASK) as usize - 1;
            }
            if entry == 0 {
                let id = self.next_name.fetch_add(1, Relaxed) as u64;
                assert!(id < ID_MASK);
                if self.names[slot].compare_exchange(0, tag | (id + 1), Relaxed, Relaxed).is_ok() {
                    return id as usize;
                }
                continue; // Lost the race: the winner may have stored this very name.
            }
            slot = (slot + 1) & ID_MASK as usize;
        }
    }

}

const BATCH: usize = 128;

/// One worker's aggregates, plus its (name id, group) pairs on their way to the lines' owners.
struct Worker {
    me: usize,
    groups: Vec<Group>,
    pending: Vec<(usize, [u32; BATCH])>,
    spare: Vec<u32>,
}

impl Worker {
    fn new(me: usize, count: usize) -> Self {
        Worker { me, groups: vec![EMPTY; count], pending: vec![(0, [0; BATCH]); count / 8 + 1], spare: Vec::new() }
    }

    #[inline(always)]
    fn mark_name(&mut self, shared: &Shared, group: usize, id: usize) {
        let line = group >> 3;
        let (len, batch) = unsafe { self.pending.get_unchecked_mut(line) };
        unsafe { *batch.get_unchecked_mut(*len) = (id as u32) << 8 | group as u32 };
        *len += 1;
        if *len == BATCH {
            *len = 0;
            shared.inboxes[line % shared.inboxes.len()].lock().unwrap().extend_from_slice(batch);
            self.drain(shared);
        }
    }

    /// Applies everything sent to this worker. Only the owner writes, so plain stores suffice.
    fn drain(&mut self, shared: &Shared) {
        std::mem::swap(&mut *shared.inboxes[self.me].lock().unwrap(), &mut self.spare);
        for &entry in &self.spare {
            let (id, group) = ((entry >> 8) as usize, (entry & 255) as usize);
            let word = unsafe { shared.seen.get_unchecked((id >> 6) * shared.stride + group) };
            word.store(word.load(Relaxed) | 1u64 << (id & 63), Relaxed);
        }
        self.spare.clear();
    }

    /// Sends what is left, then keeps applying until no worker can send any more.
    fn finish(&mut self, shared: &Shared) {
        for (line, (len, batch)) in self.pending.iter().enumerate() {
            shared.inboxes[line % shared.inboxes.len()].lock().unwrap().extend_from_slice(&batch[..*len]);
        }
        shared.finished.fetch_add(1, AcqRel);
        while shared.finished.load(Acquire) < shared.inboxes.len() {
            self.drain(shared);
            std::hint::spin_loop();
        }
        self.drain(shared);
    }
}

/// First record start at or after `from`, per the module comment.
fn record_start(input: &[u8], mut from: usize) -> usize {
    while from < input.len() {
        if input[from] == b'\n' && input.get(from + 1).is_some_and(|&b| b.is_ascii_digit() || b == 0xEF) {
            return from + 1;
        }
        from += 1;
    }
    input.len()
}

/// Aggregates the records of input[start..end] into this worker's groups.
fn parse_records(input: &[u8], start: usize, end: usize, shared: &Shared, worker: &mut Worker) {
    let regions = shared.region_group.as_ptr();
    let last_region = shared.region_group.len() as u64 - 1;
    unsafe {
        let mut p = input.as_ptr().add(start);
        let stop = input.as_ptr().add(end);
        while p < stop {
            if *p == 0xEF {
                p = p.add(3);
            }
            let (id, next) = number_field(p);
            let (name, name_raw, name_len, next) = text_field(next);
            let (_, _, note_len, next) = text_field(next);
            let (region, next) = number_field(next);

            // Amount: `-?digits.dd` of at most 8 characters after the sign, then CR or LF. Only
            // those terminators have a zero high nibble. The sign and the CR are handled without
            // branches because both are unpredictable.
            let negative = (*next == b'-') as usize;
            let amount_at = next.add(negative);
            let word = load(amount_at);
            let k = first_zero(word & 0xF0F0F0F0F0F0F0F0);
            let aligned = word << (64 - 8 * k); // Bytes 6 and 7 are the cents, byte 5 is the dot.
            let squeezed = ((aligned & 0xFF_FFFF_FFFF) << 8) | (aligned & 0xFFFF_0000_0000_0000);
            let value = digits8(squeezed) as i64;
            let amount = (value ^ -(negative as i64)) + negative as i64;
            p = amount_at.add(k + 1 + (*amount_at.add(k) == b'\r') as usize);

            let group = *regions.add(region.min(last_region) as usize) as usize;
            let g = worker.groups.get_unchecked_mut(group);
            g.count += 1;
            g.id_sum += id;
            g.amount_sum += amount;
            g.amount_min = g.amount_min.min(amount);
            g.amount_max = g.amount_max.max(amount);
            g.name_bytes += name_len as u64;
            g.note_bytes += note_len as u64;
            worker.mark_name(shared, group, shared.name_id(hash_name(name, name_raw)));
        }
    }
}

/// Reads regions.csv into a flat table. Ids ascend, so the last line sizes it. A line is
/// `id,CCC` + LF: walking it needs one word load per line and the code indexes a small array.
fn load_regions(input: &[u8]) -> (Vec<u8>, Vec<[u8; 3]>) {
    let mut codes = vec![*b"___"]; // Group 0, UNMATCHED; only its position matters.
    let mut group_of_code = [0u8; 1 << 15];
    let last = input.rsplit(|&b| b == b'\n').nth(1).unwrap();
    let max_id = last[..last.len() - 4].iter().fold(0, |v, &d| v * 10 + (d - b'0') as usize);
    let mut table = vec![UNMATCHED as u8; max_id + 2];
    let header = input.iter().position(|&b| b == b'\n').unwrap() + 1;
    unsafe {
        let mut p = input.as_ptr().add(header);
        let stop = input.as_ptr().add(input.len());
        while p < stop {
            let (id, next) = number_field(p);
            let code = [*next, *next.add(1), *next.add(2)];
            let key = code.iter().fold(0, |k, &c| k << 5 | (c & 31) as usize);
            if group_of_code[key] == 0 {
                codes.push(code);
                group_of_code[key] = (codes.len() - 1) as u8;
                assert!(codes.len() <= 256);
            }
            *table.get_unchecked_mut(id as usize) = group_of_code[key];
            p = next.add(4);
        }
    }
    (table, codes)
}

fn money(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    format!("{sign}{}.{:02}", cents.unsigned_abs() / 100, cents.unsigned_abs() % 100)
}

fn main() {
    // Huge pages round every thread's small arena up to 2 MB, which the memory score sees.
    #[cfg(target_os = "linux")]
    unsafe {
        const PR_SET_THP_DISABLE: i32 = 41;
        prctl(PR_SET_THP_DISABLE, 1, 0, 0, 0);
    }
    let data = env::var("SD_DATA").unwrap_or("/data".into());
    let out = env::var("SD_OUT").unwrap_or("/out".into());
    let input = map_file(format!("{data}/records.csv"));
    let (region_group, codes) = load_regions(map_file(format!("{data}/regions.csv")));
    let threads = thread::available_parallelism().map_or(12, |n| n.get()).min(12);
    let count = codes.len();
    let stride = count.next_multiple_of(8);
    let shared = Shared {
        region_group,
        names: lazy_table(1 << ID_BITS),
        next_name: AtomicU32::new(0),
        seen: lazy_table((1 << (ID_BITS - 6)) * stride),
        stride,
        inboxes: (0..threads).map(|_| Mutex::new(Vec::new())).collect(),
        finished: AtomicUsize::new(0),
    };

    let first = input.iter().position(|&b| b == b'\n').map_or(input.len(), |i| i + 1);
    let cut = |i: usize| match i {
        0 => first,
        i if i >= SEGMENTS => input.len(),
        i => record_start(input, (first + i * (input.len() - first) / SEGMENTS - 1).max(first)),
    };
    let cuts: Vec<usize> = (0..=SEGMENTS).map(cut).collect();
    let claim = AtomicUsize::new(0);
    let mut totals = vec![EMPTY; count];
    thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|me| {
                let (shared, cuts, claim) = (&shared, &cuts, &claim);
                scope.spawn(move || {
                    let mut worker = Worker::new(me, count);
                    loop {
                        let i = claim.fetch_add(1, Relaxed);
                        if i >= SEGMENTS {
                            worker.finish(shared);
                            return worker.groups;
                        }
                        parse_records(input, cuts[i], cuts[i + 1], shared, &mut worker);
                        // Unmapping here, in parallel, beats one thread unmapping all of it at exit,
                        // which the clock would see. Neighbours may still read just past the edges.
                        let from = (input.as_ptr() as usize + cuts[i] + 64).next_multiple_of(PAGE);
                        let to = (input.as_ptr() as usize + cuts[i + 1]) / PAGE * PAGE;
                        if to > from {
                            unsafe { munmap(from as *mut u8, to - from) };
                        }
                    }
                })
            })
            .collect();
        for worker in workers {
            for (t, g) in totals.iter_mut().zip(worker.join().unwrap()) {
                t.count += g.count;
                t.id_sum += g.id_sum;
                t.amount_sum += g.amount_sum;
                t.amount_min = t.amount_min.min(g.amount_min);
                t.amount_max = t.amount_max.max(g.amount_max);
                t.name_bytes += g.name_bytes;
                t.note_bytes += g.note_bytes;
            }
        }
    });

    let rows = (shared.next_name.load(Relaxed) as usize >> 6) + 1;
    let mut lines = BTreeMap::new();
    for (group, t) in totals.iter().enumerate().filter(|(_, t)| t.count > 0) {
        let distinct: u32 = (0..rows).map(|r| shared.seen[r * stride + group].load(Relaxed).count_ones()).sum();
        let key = if group == UNMATCHED { "_UNMATCHED".to_string() } else { String::from_utf8_lossy(&codes[group]).into_owned() };
        let line = format!(
            "{key} {} {} {} {} {} {distinct} {} {}\n",
            t.count,
            t.id_sum,
            money(t.amount_sum),
            money(t.amount_min),
            money(t.amount_max),
            t.name_bytes,
            t.note_bytes
        );
        lines.insert(key, line);
    }
    let mut output = File::create(format!("{out}/aggregates.txt")).unwrap();
    output.write_all(lines.into_values().collect::<String>().as_bytes()).unwrap();
    // Skip unmapping and destructors: the process is done and the clock is running.
    std::process::exit(0);
}
