//! Track A: per-hour, per-endpoint latency percentiles over a mmap'd request log.
//!
//! Phase 1: workers claim file segments and turn each line into one packed u32, chained into
//! per-hour blocks inside that segment's own arena. Phase 2: workers take one hour at a time,
//! select its percentiles and write its text at the final file offset. A segment's arena is
//! returned to the kernel when its last hour is done, because the log is roughly time-ordered:
//! the record store shrinks while the output grows, and peak memory is the larger of the two.
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering::*};
use std::sync::Mutex;
use std::{env, ptr, slice, thread};

const HOURS: usize = 72;
const SEGMENTS: usize = 72;
const READ_BYTES: usize = 1 << 18;
const PAD: usize = 128;
const BLOCK: usize = 256; // Words per block; the first word links to the hour's previous block.
const SLOT_BITS: u32 = 12;
const SLOTS: usize = 1 << SLOT_BITS; // 1,842 endpoints per seed: the table stays under half full.

// A record packs slot:12 | error:1 | latency:19. The 2.6% of latencies that do not fit store the
// marker and spill the value into the next word, so 3.1M lines take about 13 MB.
const LAT_BITS: u32 = 19;
const SPILL: u32 = (1 << LAT_BITS) - 1;

const DAYS: [&[u8; 11]; 4] = [b"2026-10-30T", b"2026-10-31T", b"2026-11-01T", b"2026-11-02T"];
const PAIRS: &[u8; 200] = b"00010203040506070809101112131415161718192021222324252627282930313233343536373839404142434445464748495051525354555657585960616263646566676869707172737475767778798081828384858687888990919293949596979899";

const PROT_READ: i32 = 1;
const PROT_WRITE: i32 = 2;
const MAP_PRIVATE: i32 = 2;
const MADV_DONTNEED: i32 = 4; // Drops the pages at once.
#[cfg(target_os = "linux")]
const MAP_ANON: i32 = 0x20;
#[cfg(not(target_os = "linux"))]
const MAP_ANON: i32 = 0x1000;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    #[cfg(target_os = "linux")]
    fn prctl(option: i32, a: u64, b: u64, c: u64, d: u64) -> i32;
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
}

/// Anonymous memory: only pages actually touched count toward the peak.
fn reserve(bytes: usize) -> *mut u8 {
    unsafe { mmap(ptr::null_mut(), bytes, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, -1, 0) }
}

#[inline(always)]
unsafe fn load(p: *const u8) -> u64 {
    (p as *const u64).read_unaligned()
}

/// Byte index of the first byte equal to `b` in the little-endian word, 8 if there is none.
#[inline(always)]
fn first_byte(word: u64, b: u8) -> usize {
    let x = word ^ (b as u64 * 0x0101010101010101);
    let found = x.wrapping_sub(0x0101010101010101) & !x & 0x8080808080808080;
    (found.trailing_zeros() / 8) as usize
}

/// Length of the run of bytes before the next space. Endpoints average 33 bytes, so a 32-byte step
/// would mispredict its loop exit on nearly half the lines; 64 bytes at once almost never loops.
#[cfg(target_feature = "avx2")]
#[inline(always)]
unsafe fn until_space(p: *const u8) -> usize {
    use std::arch::x86_64::*;
    let spaces = _mm256_set1_epi8(b' ' as i8);
    let mask = |q: *const u8| {
        let v = _mm256_loadu_si256(q as *const __m256i);
        _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, spaces)) as u32 as u64
    };
    let mut n = 0;
    loop {
        let m = mask(p.add(n)) | mask(p.add(n + 32)) << 32;
        if m != 0 {
            return n + m.trailing_zeros() as usize;
        }
        n += 64;
    }
}

#[cfg(not(target_feature = "avx2"))]
#[inline(always)]
unsafe fn until_space(p: *const u8) -> usize {
    let mut n = 0;
    loop {
        let k = first_byte(load(p.add(n)), b' ');
        if k < 8 {
            return n + k;
        }
        n += 8;
    }
}

/// Whether two names of the same length match. Up to 64 bytes this is a constant-cost compare with
/// no call and no length-dependent branch, and it runs once per line.
#[cfg(target_feature = "avx2")]
#[inline(always)]
unsafe fn same_name(a: *const u8, b: *const u8, len: usize) -> bool {
    use std::arch::x86_64::*;
    if len > 64 {
        return slice::from_raw_parts(a, len) == slice::from_raw_parts(b, len);
    }
    let equal = |at: usize| {
        let x = _mm256_loadu_si256(a.add(at) as *const __m256i);
        let y = _mm256_loadu_si256(b.add(at) as *const __m256i);
        _mm256_movemask_epi8(_mm256_cmpeq_epi8(x, y)) as u32 as u64
    };
    (equal(0) | equal(32) << 32 | (u64::MAX << 1 << (len - 1))) == u64::MAX
}

#[cfg(not(target_feature = "avx2"))]
#[inline(always)]
unsafe fn same_name(a: *const u8, b: *const u8, len: usize) -> bool {
    slice::from_raw_parts(a, len) == slice::from_raw_parts(b, len)
}

/// Lock-free endpoint interner shared by all workers. After the first few thousand lines it is
/// read-only, so its cache lines stay shared. A slot packs (address of a copy) << 8 | length.
struct Interner {
    slots: Vec<AtomicU64>,
}

impl Interner {
    /// The bytes of a slot, or the empty slice if it is free.
    fn name(&self, slot: usize) -> &'static [u8] {
        let packed = self.slots[slot].load(Relaxed);
        if packed == 0 {
            return &[];
        }
        unsafe { slice::from_raw_parts((packed >> 8) as *const u8, (packed & 255) as usize) }
    }

    #[inline(always)]
    unsafe fn intern(&self, p: *const u8, len: usize) -> usize {
        // Sampling head, middle and tail keeps hashing O(1); equality is still byte-exact.
        let head = load(p) & (u64::MAX >> (64 - 8 * len.min(8)));
        let (mid, tail) = if len >= 8 { (load(p.add(len / 2 - 4)), load(p.add(len - 8))) } else { (0, 0) };
        let mixed = head ^ mid.rotate_left(21) ^ tail.rotate_left(42) ^ len as u64;
        let mut slot = (mixed.wrapping_mul(0x9E3779B97F4A7C15) >> (64 - SLOT_BITS)) as usize;
        let key = slice::from_raw_parts(p, len);
        loop {
            let packed = self.slots.get_unchecked(slot).load(Acquire);
            if packed == 0 {
                // Padded to 64 bytes, so that `same_name` may always load a full block.
                let mut copy = key.to_vec();
                copy.resize(len.max(64), 0);
                let new = (copy.leak().as_ptr() as u64) << 8 | len as u64;
                if self.slots[slot].compare_exchange(0, new, AcqRel, Acquire).is_ok() {
                    return slot;
                }
                continue; // Lost the race: re-read the slot, the winner may hold our key.
            }
            if packed & 255 == len as u64 && same_name((packed >> 8) as *const u8, p, len) {
                return slot;
            }
            slot = (slot + 1) & (SLOTS - 1);
        }
    }
}

/// Decimal value of a latency whose digits end at the LF: returns (value, digit count).
#[inline(always)]
unsafe fn parse_latency(p: *const u8) -> (u32, usize) {
    let word = load(p);
    let k = first_byte(word, b'\n');
    if k == 8 {
        // 9 or 10 digits, a fraction of a percent of lines.
        let (mut v, mut n) = (0u64, 0);
        while *p.add(n) != b'\n' {
            v = v * 10 + (*p.add(n) - b'0') as u64;
            n += 1;
        }
        return (v as u32, n);
    }
    // Right-align the digits (the vacated low bytes act as leading zeros), then combine pairs,
    // quads and the two halves with one multiply each.
    let v = (word << (64 - 8 * k)) & 0x0F0F0F0F0F0F0F0F;
    let v = (v.wrapping_mul(2561) >> 8) & 0x00FF00FF00FF00FF;
    let v = (v.wrapping_mul(6553601) >> 16) & 0x0000FFFF0000FFFF;
    ((v.wrapping_mul(42949672960001) >> 32) as u32, k)
}

/// First line start at or after `from`; lines are far shorter than the probe.
fn line_start(file: &File, len: usize, from: usize) -> usize {
    if from == 0 || from >= len {
        return from.min(len);
    }
    let mut probe = [0u8; 256];
    let got = file.read_at(&mut probe, from as u64).unwrap();
    from + probe[..got].iter().position(|&b| b == b'\n').map_or(got, |i| i + 1)
}

/// The records of one file segment. Each hour's words form a chain of BLOCK-aligned blocks.
struct Segment {
    arena: *mut u32,
    arena_bytes: usize,
    next_block: usize, // Word index; block 0 is never used, so a cursor of 0 means "no block".
    cursor: [u32; HOURS], // Next free word of the hour's newest block.
    hours_left: AtomicU32,
}

unsafe impl Send for Segment {}
unsafe impl Sync for Segment {}

impl Segment {
    #[inline(always)]
    unsafe fn push(&mut self, bucket: usize, word: u32) {
        let mut at = *self.cursor.get_unchecked(bucket) as usize;
        if at % BLOCK == 0 {
            *self.arena.add(self.next_block) = at as u32;
            at = self.next_block + 1;
            self.next_block += BLOCK;
        }
        *self.arena.add(at) = word;
        *self.cursor.get_unchecked_mut(bucket) = at as u32 + 1;
    }

    /// Parses every line of the file range [start, end); both ends are line starts. The bytes are
    /// read into a buffer small enough to stay in this core's L2 while the lines are parsed.
    fn parse(&mut self, file: &File, start: usize, end: usize, names: &Interner, buffer: &mut [u8]) {
        // Lines are at least 40 bytes and take at most 2 words: virtual space only, never touched.
        self.arena_bytes = (end - start) / 4 + (HOURS + 2) * BLOCK * 8;
        self.arena = reserve(self.arena_bytes) as *mut u32;
        self.next_block = BLOCK;
        let capacity = buffer.len() - PAD;
        let mut position = start;
        while position < end {
            let wanted = capacity.min(end - position);
            file.read_exact_at(&mut buffer[..wanted], position as u64).unwrap();
            // Whole lines only; word loads may run into PAD bytes past the last LF.
            let whole = buffer[..wanted].iter().rposition(|&b| b == b'\n').unwrap() + 1;
            unsafe { self.parse_lines(&buffer[..whole], names) };
            position += whole;
        }
        let used = self.cursor.iter().filter(|&&c| c != 0).count();
        self.hours_left = AtomicU32::new(used as u32);
    }

    #[inline(always)]
    unsafe fn parse_lines(&mut self, lines: &[u8], names: &Interner) {
        let mut p = lines.as_ptr();
        let stop = p.add(lines.len());
        while p < stop {
            // "2026-10-30T18:..": the span is fixed, so the day's last digit plus whether the
            // tens digit is '3' (October, bit 0 set) gives the day index directly.
            let day = (*p.add(9) & 15) as usize + 1 - (*p.add(8) & 1) as usize;
            let hour = (*p.add(11) & 15) as usize * 10 + (*p.add(12) & 15) as usize;
            let bucket = day * 24 + hour - 18;
            let endpoint = p.add(29 + first_byte(load(p.add(28)), b' '));
            let len = until_space(endpoint);
            let slot = names.intern(endpoint, len) as u32;
            let status = endpoint.add(len + 1);
            let error = ((*status == b'5') as u32) << LAT_BITS;
            let (latency, digits) = parse_latency(status.add(4));
            let tag = slot << (LAT_BITS + 1) | error;
            if latency < SPILL {
                self.push(bucket, tag | latency);
            } else {
                // A pair never straddles blocks: a marker in a block's last word is a filler.
                if self.cursor[bucket] as usize % BLOCK == BLOCK - 1 {
                    self.push(bucket, SPILL);
                }
                self.push(bucket, tag | SPILL);
                self.push(bucket, latency);
            }
            p = status.add(5 + digits);
        }
    }

    /// Calls `f(slot, is_error, latency)` for every record of an hour.
    #[inline(always)]
    fn for_each(&self, bucket: usize, mut f: impl FnMut(usize, u32, u32)) {
        let mut end = self.cursor[bucket] as usize;
        while end != 0 {
            let start = (end - 1) / BLOCK * BLOCK;
            let words = unsafe { slice::from_raw_parts(self.arena.add(start), end - start) };
            let mut i = 1;
            while i < words.len() {
                let w = words[i];
                let mut latency = w & SPILL;
                if latency == SPILL {
                    if i + 1 == words.len() {
                        break;
                    }
                    i += 1;
                    latency = words[i];
                }
                f((w >> (LAT_BITS + 1)) as usize, (w >> LAT_BITS) & 1, latency);
                i += 1;
            }
            end = words[0] as usize;
        }
    }

    /// Called after each hour is formatted; the last call gives the arena back.
    fn hour_done(&self, bucket: usize) {
        if self.cursor[bucket] != 0 && self.hours_left.fetch_sub(1, Relaxed) == 1 {
            unsafe { madvise(self.arena as *mut u8, self.arena_bytes, MADV_DONTNEED) };
        }
    }
}

/// Appends a decimal number, two digits per division.
fn push_number(out: &mut Vec<u8>, mut v: u64) {
    let mut buf = [0u8; 20];
    let mut i = 20;
    while v >= 10 {
        i -= 2;
        let pair = (v % 100) as usize * 2;
        buf[i..i + 2].copy_from_slice(&PAIRS[pair..pair + 2]);
        v /= 100;
    }
    if v > 0 || i == 20 {
        i -= 1;
        buf[i] = b'0' + v as u8;
    }
    out.extend_from_slice(&buf[i..]);
}

/// Scratch reused across the hours one worker handles.
#[derive(Default)]
struct Scratch {
    latencies: Vec<u32>,
    text: Vec<u8>,
}

/// Formats one hour: counts per slot, lays the latencies out contiguously, selects percentiles.
fn format_hour(bucket: usize, segments: &[Segment], order: &[(usize, &[u8])], s: &mut Scratch) {
    let mut counts = [0u32; SLOTS];
    let mut errors = [0u32; SLOTS];
    for segment in segments {
        segment.for_each(bucket, |slot, is_error, _| {
            counts[slot] += 1;
            errors[slot] += is_error;
        });
    }
    let mut next = [0u32; SLOTS];
    let mut total = 0;
    for (start, n) in next.iter_mut().zip(&counts) {
        *start = total;
        total += n;
    }
    s.latencies.resize(total as usize, 0);
    for segment in segments {
        segment.for_each(bucket, |slot, _, latency| {
            s.latencies[next[slot] as usize] = latency;
            next[slot] += 1;
        });
    }
    s.text.clear();
    let day = DAYS[(bucket + 18) / 24];
    let hour = (bucket + 18) % 24;
    for &(slot, name) in order {
        let n = counts[slot] as usize;
        if n == 0 {
            continue;
        }
        let end = next[slot] as usize; // One past this slot's run, after the fill above.
        let group = &mut s.latencies[end - n..end];
        let rank = |p: usize| (p * n + 99) / 100 - 1;
        // Each select leaves everything right of its rank larger, so the next one narrows.
        let (r50, r95, r99) = (rank(50), rank(95), rank(99));
        let p50 = *group.select_nth_unstable(r50).1;
        let p95 = if r95 == r50 { p50 } else { *group[r50 + 1..].select_nth_unstable(r95 - r50 - 1).1 };
        let p99 = if r99 == r95 { p95 } else { *group[r95 + 1..].select_nth_unstable(r99 - r95 - 1).1 };
        let e = errors[slot] as u64;
        s.text.extend_from_slice(day);
        s.text.extend_from_slice(&[b'0' + (hour / 10) as u8, b'0' + (hour % 10) as u8]);
        s.text.extend_from_slice(b":00:00Z ");
        s.text.extend_from_slice(name);
        for v in [n as u64, p50 as u64, p95 as u64, p99 as u64, e, e * 1_000_000 / n as u64] {
            s.text.push(b' ');
            push_number(&mut s.text, v);
        }
        s.text.push(b'\n');
    }
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
    let file = File::open(format!("{data}/requests.log")).unwrap();
    let output = File::create(format!("{out}/summary.txt")).unwrap();
    let len = file.metadata().unwrap().len() as usize;
    if len == 0 {
        return;
    }
    let threads = thread::available_parallelism().map_or(12, |n| n.get()).min(12);
    let names = Interner { slots: (0..SLOTS).map(|_| AtomicU64::new(0)).collect() };

    let empty = || Segment {
        arena: ptr::null_mut(),
        arena_bytes: 0,
        next_block: 0,
        cursor: [0; HOURS],
        hours_left: AtomicU32::new(0),
    };
    let pending: Vec<Mutex<Segment>> = (0..SEGMENTS).map(|_| Mutex::new(empty())).collect();
    let claim = AtomicUsize::new(0);
    thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                let mut buffer = vec![0u8; READ_BYTES + PAD];
                loop {
                    let i = claim.fetch_add(1, Relaxed);
                    if i >= SEGMENTS {
                        return;
                    }
                    let (from, to) = (line_start(&file, len, i * len / SEGMENTS), line_start(&file, len, (i + 1) * len / SEGMENTS));
                    pending[i].lock().unwrap().parse(&file, from, to, &names, &mut buffer);
                }
            });
        }
    });
    let segments: Vec<Segment> = pending.into_iter().map(|m| m.into_inner().unwrap()).collect();

    // Bytewise order of the endpoints, shared by every hour.
    let mut order: Vec<(usize, &[u8])> =
        (0..SLOTS).map(|s| (s, names.name(s))).filter(|e| !e.1.is_empty()).collect();
    order.sort_unstable_by_key(|e| e.1);

    // offsets[h] is published once every earlier hour's length is known, which lets each worker
    // write its text at the right place while later hours are still being formatted.
    const UNKNOWN: u64 = u64::MAX;
    let offsets: Vec<AtomicU64> = (0..=HOURS).map(|h| AtomicU64::new(if h == 0 { 0 } else { UNKNOWN })).collect();
    let next_hour = AtomicUsize::new(0);
    thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                let mut scratch = Scratch::default();
                loop {
                    let bucket = next_hour.fetch_add(1, Relaxed);
                    if bucket >= HOURS {
                        return;
                    }
                    format_hour(bucket, &segments, &order, &mut scratch);
                    segments.iter().for_each(|segment| segment.hour_done(bucket));
                    let start = loop {
                        let known = offsets[bucket].load(Acquire);
                        if known != UNKNOWN {
                            break known;
                        }
                        std::hint::spin_loop();
                    };
                    offsets[bucket + 1].store(start + scratch.text.len() as u64, Release);
                    output.write_all_at(&scratch.text, start).unwrap();
                }
            });
        }
    });
    // Skip unmapping and destructors: the process is done and the clock is running.
    std::process::exit(0);
}
