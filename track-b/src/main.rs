//! Track B: 50,000 boolean and phrase queries over a mmap'd corpus, with no global index at all.
//!
//! The queries are known up front, so the corpus is cut into chunks of a few thousand documents and
//! each worker handles one chunk end to end: tokenise it, keep only postings of query terms, answer
//! every BOOL query on that chunk with small bitsets, and verify each phrase at the occurrences of
//! its rarest term by reading the text around them. Memory is bounded by the chunk, not the corpus.
use std::fs::File;
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering::*};
use std::sync::Mutex;
use std::{env, ptr, slice, thread};

const CHUNK_BYTES: usize = 1 << 21;
const REL_BITS: u32 = 12; // A 2 MB chunk holds about 2,000 documents; asserted to fit in 4,096.
const SAMPLE_BYTES: usize = 1 << 18;
const READ_BYTES: usize = 1 << 18;
const PAD: usize = 128;
const NONE: u32 = u32::MAX;
// A term this common in the sample skips the log: its bitset is written directly during the scan.
const FREQUENT: u32 = 8;
const STRIDE: usize = 1 << (REL_BITS - 6); // Words per bitset row, enough for any chunk.

const PROT_READ: i32 = 1;
const MAP_PRIVATE: i32 = 2;
const MAP_FIXED: i32 = 0x10;
#[cfg(target_os = "linux")]
const MAP_ANON: i32 = 0x20;
#[cfg(not(target_os = "linux"))]
const MAP_ANON: i32 = 0x1000;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    #[cfg(target_os = "linux")]
    fn prctl(option: i32, a: u64, b: u64, c: u64, d: u64) -> i32;
}

/// Maps a file with one zeroed page behind it, so block reads may run past the end of the data.
fn map_file(path: String) -> &'static [u8] {
    let file = File::open(path).unwrap();
    let len = file.metadata().unwrap().len() as usize;
    unsafe {
        let base = mmap(ptr::null_mut(), len + 128 * 1024, PROT_READ, MAP_PRIVATE | MAP_ANON, -1, 0);
        if len > 0 {
            assert_eq!(mmap(base, len, PROT_READ, MAP_PRIVATE | MAP_FIXED, file.as_raw_fd(), 0), base);
        }
        slice::from_raw_parts(base, len)
    }
}

#[inline(always)]
unsafe fn load(p: *const u8) -> u64 {
    (p as *const u64).read_unaligned()
}

const LOWER: u64 = 0x2020202020202020;

/// A token's identity for lookup: its first and last eight bytes lowercased, plus its length.
/// `| 0x20` lowercases letters and leaves digits alone, and tokens hold nothing else.
#[inline(always)]
unsafe fn token_key(p: *const u8, len: usize) -> (u64, u64) {
    let head = (load(p) | LOWER) & (u64::MAX >> (64 - 8 * len.min(8)));
    let tail = if len > 8 { load(p.add(len - 8)) | LOWER } else { 0 };
    (head, tail)
}

#[inline(always)]
fn is_token_byte(b: u8) -> bool {
    b.is_ascii_digit() || (b | 0x20).is_ascii_lowercase()
}

/// Bits set at token bytes and at LFs, for the 64 bytes at `p`.
#[cfg(target_feature = "avx2")]
#[inline(always)]
unsafe fn classify(p: *const u8) -> (u64, u64) {
    use std::arch::x86_64::*;
    let half = |q: *const u8| {
        let v = _mm256_loadu_si256(q as *const __m256i);
        // Shift each class up to the top of the signed range; one compare then tests both ends.
        let lower = _mm256_or_si256(v, _mm256_set1_epi8(0x20));
        let letters = _mm256_cmpgt_epi8(_mm256_add_epi8(lower, _mm256_set1_epi8(5)), _mm256_set1_epi8(0x65));
        let digits = _mm256_cmpgt_epi8(_mm256_add_epi8(v, _mm256_set1_epi8(0x46)), _mm256_set1_epi8(0x75));
        let token = _mm256_movemask_epi8(_mm256_or_si256(letters, digits)) as u32 as u64;
        let lf = _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, _mm256_set1_epi8(b'\n' as i8))) as u32 as u64;
        (token, lf)
    };
    let (a, b) = (half(p), half(p.add(32)));
    (a.0 | b.0 << 32, a.1 | b.1 << 32)
}

#[cfg(not(target_feature = "avx2"))]
#[inline(always)]
unsafe fn classify(p: *const u8) -> (u64, u64) {
    let (mut token, mut lf) = (0, 0);
    for i in 0..64 {
        let b = *p.add(i);
        token |= (is_token_byte(b) as u64) << i;
        lf |= ((b == b'\n') as u64) << i;
    }
    (token, lf)
}

/// Calls `f(start, end)` for every token of the document text at `p`; returns the byte after its LF.
#[inline(always)]
unsafe fn for_each_token(mut p: *const u8, mut f: impl FnMut(*const u8, *const u8)) -> *const u8 {
    let mut open: *const u8 = ptr::null();
    loop {
        let (mut token, lf) = classify(p);
        let ends = lf != 0;
        if ends {
            token &= (1u64 << lf.trailing_zeros()).wrapping_sub(1);
        }
        // Each set bit is a boundary: starts and ends alternate.
        let mut edges = token ^ (token << 1 | !open.is_null() as u64);
        while edges != 0 {
            let at = p.add(edges.trailing_zeros() as usize);
            edges &= edges - 1;
            if open.is_null() {
                open = at;
            } else {
                f(open, at);
                open = ptr::null();
            }
        }
        if ends {
            let lf_at = p.add(lf.trailing_zeros() as usize);
            if !open.is_null() {
                f(open, lf_at); // A token running up to a LF in the block's last byte.
            }
            return lf_at.add(1);
        }
        p = p.add(64);
    }
}

const TERM_BITS: u32 = 18;
const TABLE_BITS: u32 = 17;

/// Every distinct query term, addressed by a hash of the token. One u64 per slot, a table of 1 MB,
/// because every token probes it and it must stay in a core's 1.25 MB L2. A slot keeps 46 more hash
/// bits beside term + 1, so a match means 63 bits agree: about 5e-9 chance of any false match in a
/// run, given 610,000 distinct tokens against 76,000 terms.
struct Terms {
    slots: Vec<u64>,
    text: Vec<&'static [u8]>,
}

/// Mixes a token's key: head, tail and length, plus the middle for tokens over 16 bytes (rare).
#[inline(always)]
unsafe fn token_hash(p: *const u8, len: usize) -> u64 {
    let (head, tail) = token_key(p, len);
    let mut h = head ^ tail.rotate_left(23).wrapping_mul(0xC2B2AE3D27D4EB4F) ^ len as u64;
    if len > 16 {
        h = hash_middle(h, p, len);
    }
    h = h.wrapping_mul(0x9E3779B97F4A7C15);
    h ^ (h >> 29)
}

#[cold]
unsafe fn hash_middle(mut h: u64, p: *const u8, len: usize) -> u64 {
    for i in 8..len - 8 {
        h = (h ^ (*p.add(i) | 0x20) as u64).wrapping_mul(0x100000001B3);
    }
    h
}

impl Terms {
    fn intern(&mut self, word: &'static [u8]) -> u32 {
        let h = unsafe { token_hash(word.as_ptr(), word.len()) };
        let mut i = (h >> (64 - TABLE_BITS)) as usize;
        loop {
            let slot = self.slots[i];
            if slot == 0 {
                self.text.push(word);
                self.slots[i] = h << TERM_BITS | self.text.len() as u64;
                return self.text.len() as u32 - 1;
            }
            if slot >> TERM_BITS == h << TERM_BITS >> TERM_BITS {
                return (slot & ((1 << TERM_BITS) - 1)) as u32 - 1;
            }
            i = (i + 1) & (self.slots.len() - 1);
        }
    }

    /// The term a token is, or NONE.
    #[inline(always)]
    unsafe fn find(&self, p: *const u8, len: usize) -> u32 {
        let h = token_hash(p, len);
        let mut i = (h >> (64 - TABLE_BITS)) as usize;
        loop {
            let slot = *self.slots.get_unchecked(i);
            if slot >> TERM_BITS == h << TERM_BITS >> TERM_BITS && slot != 0 {
                return (slot & ((1 << TERM_BITS) - 1)) as u32 - 1;
            }
            if slot == 0 {
                return NONE;
            }
            i = (i + 1) & (self.slots.len() - 1);
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Op {
    And,
    Or,
    Not,
}

/// A phrase, kept as its terms plus which one to look for in the text.
struct Phrase {
    query: u32,
    terms: Vec<u32>,
    anchor_at: usize,
}

struct Queries {
    terms: Terms,
    /// Flat (op, term) lists of the BOOL queries; each query's first op is Or onto an empty set.
    bool_ops: Vec<(Op, u32)>,
    /// (query index, first op, op count), in query order.
    bools: Vec<(u32, u32, u32)>,
    phrases: Vec<Phrase>,
    total: usize,
}

fn parse_queries(text: &'static [u8]) -> Queries {
    let mut q = Queries {
        terms: Terms { slots: vec![0; 1 << TABLE_BITS], text: Vec::new() },
        bool_ops: Vec::new(),
        bools: Vec::new(),
        phrases: Vec::new(),
        total: 0,
    };
    for (index, line) in text.split(|&b| b == b'\n').filter(|l| !l.is_empty()).enumerate() {
        q.total += 1;
        if line.starts_with(b"BOOL ") {
            let first = q.bool_ops.len() as u32;
            let mut op = Op::Or;
            for (i, word) in line[5..].split(|&b| b == b' ').enumerate() {
                if i % 2 == 0 {
                    q.bool_ops.push((op, q.terms.intern(word)));
                } else {
                    op = match word {
                        b"AND" => Op::And,
                        b"OR" => Op::Or,
                        _ => Op::Not,
                    };
                }
            }
            q.bools.push((index as u32, first, q.bool_ops.len() as u32 - first));
        } else {
            let words = line[8..line.len() - 1].split(|&b| b == b' ');
            let terms = words.map(|w| q.terms.intern(w)).collect();
            q.phrases.push(Phrase { query: index as u32, terms, anchor_at: 0 });
        }
    }
    assert!(q.total <= 1 << 16);
    q
}

/// How often each term occurs in a slice of documents, to tell rare terms from common ones.
fn sample_frequencies(q: &Queries, sample: &[u8]) -> Vec<u32> {
    let mut seen = vec![0u32; q.terms.text.len()];
    unsafe {
        let mut p = sample.as_ptr();
        let stop = p.add(sample.len());
        while p < stop {
            while *p != b'\t' {
                p = p.add(1);
            }
            p = for_each_token(p.add(1), |a, b| {
                let term = q.terms.find(a, b as usize - a as usize);
                if term != NONE {
                    seen[term as usize] += 1;
                }
            });
        }
    }
    seen
}

/// What a worker keeps per term during a chunk.
const BOOL_TERM: u32 = 1 << 31;
const ANCHOR_TERM: u32 = 1 << 30;

/// Tables derived from the queries, shared by all workers.
struct Plan {
    /// Per term: flags and its dense index among the BOOL terms.
    role: Vec<u32>,
    bool_terms: usize,
    /// BOOL terms below this index are the frequent ones, and their index is their bitset row.
    frequent: usize,
    /// Per term: range in `anchored` of the phrases anchored on it.
    anchored_at: Vec<u32>,
    anchored: Vec<u32>,
}

fn make_plan(q: &mut Queries, frequency: &[u32]) -> Plan {
    let n = q.terms.text.len();
    let mut role = vec![0u32; n];
    let mut frequent = 0;
    let mut bool_terms = 0;
    for pass in [true, false] {
        for op in &q.bool_ops {
            let term = op.1 as usize;
            if role[term] == 0 && (frequency[term] >= FREQUENT) == pass {
                role[term] = BOOL_TERM | bool_terms;
                bool_terms += 1;
            }
        }
        if pass {
            frequent = bool_terms as usize;
        }
    }
    for op in &mut q.bool_ops {
        op.1 = role[op.1 as usize] & !(BOOL_TERM | ANCHOR_TERM);
    }
    let text = &q.terms.text;
    let mut counts = vec![0u32; n + 1];
    for phrase in &mut q.phrases {
        // Rarest in the sample; among terms it never saw, the longest word is the best guess.
        let rank = |t: u32| (frequency[t as usize], usize::MAX - text[t as usize].len());
        phrase.anchor_at = (0..phrase.terms.len()).min_by_key(|&i| rank(phrase.terms[i])).unwrap();
        let anchor = phrase.terms[phrase.anchor_at] as usize;
        role[anchor] |= ANCHOR_TERM;
        counts[anchor + 1] += 1;
    }
    for i in 0..n {
        counts[i + 1] += counts[i];
    }
    let mut anchored = vec![0u32; q.phrases.len()];
    let mut cursor = counts.clone();
    for (i, phrase) in q.phrases.iter().enumerate() {
        let anchor = phrase.terms[phrase.anchor_at] as usize;
        anchored[cursor[anchor] as usize] = i as u32;
        cursor[anchor] += 1;
    }
    Plan { role, frequent, bool_terms: bool_terms as usize, anchored_at: counts, anchored }
}

/// One worker's reusable buffers.
struct Worker {
    last_doc: Vec<u16>,
    log: Vec<u32>,
    buffer: Vec<u8>,
    counts: Vec<u32>,
    list: Vec<u16>,
    dense_of: Vec<u32>,
    dense: Vec<u64>,
    result: Vec<u64>,
    scratch: Vec<u64>,
    emitted: Vec<u8>,
    hits: Vec<(u32, u32)>,
    last_hit: Vec<u32>,
}

/// One chunk's first-ten candidates: query << 16 | relative document, ascending by query.
struct ChunkFirsts {
    base: u32,
    entries: Vec<u32>,
}

struct Shared<'a> {
    q: &'a Queries,
    plan: &'a Plan,
    counts: Vec<AtomicU32>,
    sums: Vec<AtomicU64>,
}

#[inline(always)]
fn set_bit(words: &mut [u64], at: u16) {
    unsafe { *words.get_unchecked_mut(at as usize >> 6) |= 1 << (at & 63) };
}

impl Worker {
    fn new(q: &Queries, plan: &Plan) -> Self {
        Worker {
            last_doc: vec![0; plan.bool_terms],
            log: Vec::new(),
            buffer: vec![0; READ_BYTES + 2 * PAD],
            counts: vec![0; plan.bool_terms + 1],
            list: Vec::new(),
            dense_of: vec![NONE; plan.bool_terms],
            dense: Vec::new(),
            result: Vec::new(),
            scratch: Vec::new(),
            emitted: vec![0; q.total],
            hits: Vec::new(),
            last_hit: vec![NONE; q.phrases.len()],
        }
    }

    /// Tokenises the file range [start, end) of whole documents and returns the base document id and
    /// the document count. The bytes are read in pieces that stay in this core's L2. Phrases are
    /// verified on the spot, while the text around the occurrence is still in the buffer.
    fn scan(&mut self, file: &File, start: usize, end: usize, shared: &Shared) -> (u32, usize) {
        let Shared { q, plan, .. } = shared;
        self.log.clear();
        self.last_doc.fill(0);
        self.dense.clear();
        self.dense.resize(plan.frequent * STRIDE, 0);
        let mut base = 0;
        let mut docs = 0usize;
        let mut position = start;
        let mut buffer = std::mem::take(&mut self.buffer);
        let capacity = buffer.len() - 2 * PAD;
        while position < end {
            let wanted = capacity.min(end - position);
            // A LF in front means reading backwards from the first document's text stops in time.
            buffer[PAD - 1] = b'\n';
            file.read_exact_at(&mut buffer[PAD..PAD + wanted], position as u64).unwrap();
            let whole = buffer[PAD..PAD + wanted].iter().rposition(|&b| b == b'\n').unwrap() + 1;
            unsafe {
                let mut p = buffer.as_ptr().add(PAD);
                let stop = p.add(whole);
                while p < stop {
                    let mut id = 0u32;
                    while *p != b'\t' {
                        id = id * 10 + (*p - b'0') as u32;
                        p = p.add(1);
                    }
                    if docs == 0 {
                        base = id;
                    }
                    docs += 1;
                    let rel = id - base;
                    assert!(rel >> REL_BITS == 0);
                    let (log, last_doc, last_hit, hits, dense) =
                        (&mut self.log, &mut self.last_doc, &mut self.last_hit, &mut self.hits, &mut self.dense);
                    p = for_each_token(p.add(1), |a, b| {
                        let term = q.terms.find(a, b as usize - a as usize);
                        if term == NONE {
                            return;
                        }
                        let role = *plan.role.get_unchecked(term as usize);
                        if role & BOOL_TERM != 0 {
                            let index = role & !(BOOL_TERM | ANCHOR_TERM);
                            if (index as usize) < plan.frequent {
                                *dense.get_unchecked_mut(index as usize * STRIDE + (rel as usize >> 6)) |= 1 << (rel & 63);
                            } else {
                                let last = last_doc.get_unchecked_mut(index as usize);
                                if *last != rel as u16 + 1 {
                                    *last = rel as u16 + 1;
                                    log.push(index << REL_BITS | rel);
                                }
                            }
                        }
                        if role & ANCHOR_TERM != 0 {
                            let from = plan.anchored_at[term as usize] as usize;
                            let to = plan.anchored_at[term as usize + 1] as usize;
                            for &index in &plan.anchored[from..to] {
                                let phrase = &q.phrases[index as usize];
                                if last_hit[index as usize] != id && phrase_at(q, phrase, a) {
                                    last_hit[index as usize] = id;
                                    hits.push((phrase.query, id));
                                }
                            }
                        }
                    });
                }
            }
            position += whole;
        }
        self.buffer = buffer;
        (base, docs)
    }

    /// Groups the log by term: each term's documents become a sorted u16 list, or a bitset if it
    /// has enough of them that word-at-a-time operations win.
    fn invert(&mut self, words: usize, frequent: usize) {
        self.counts.fill(0);
        for &entry in &self.log {
            self.counts[(entry >> REL_BITS) as usize + 1] += 1;
        }
        let mut total = 0;
        for t in 0..self.dense_of.len() {
            let count = self.counts[t + 1];
            self.dense_of[t] = NONE;
            if t < frequent {
                self.dense_of[t] = t as u32;
            } else if count as usize >= words {
                self.dense_of[t] = (self.dense.len() / STRIDE) as u32;
                self.dense.resize(self.dense.len() + STRIDE, 0);
            }
            self.counts[t + 1] = total;
            total += count;
        }
        self.list.resize(total as usize, 0);
        for &entry in &self.log {
            let t = (entry >> REL_BITS) as usize;
            let rel = (entry & ((1 << REL_BITS) - 1)) as u16;
            let at = self.dense_of[t];
            if at != NONE {
                set_bit(&mut self.dense[at as usize * STRIDE..][..words], rel);
            }
            self.list[self.counts[t + 1] as usize] = rel;
            self.counts[t + 1] += 1;
        }
    }

    /// Evaluates one BOOL query on this chunk into `result`, strictly left to right.
    fn evaluate(&mut self, ops: &[(Op, u32)], words: usize) -> bool {
        self.result[..words].fill(0);
        let mut any = false;
        for &(op, t) in ops {
            let (from, to) = (if t == 0 { 0 } else { self.counts[t as usize] }, self.counts[t as usize + 1]);
            let docs = &self.list[from as usize..to as usize];
            let at = self.dense_of[t as usize];
            match op {
                Op::Or => {
                    if at != NONE {
                        let bits = &self.dense[at as usize * STRIDE..][..words];
                        self.result[..words].iter_mut().zip(bits).for_each(|(r, b)| *r |= b);
                    } else {
                        docs.iter().for_each(|&d| set_bit(&mut self.result, d));
                    }
                    any |= at != NONE || !docs.is_empty();
                }
                Op::And if any => {
                    if at != NONE {
                        let bits = &self.dense[at as usize * STRIDE..][..words];
                        self.result[..words].iter_mut().zip(bits).for_each(|(r, b)| *r &= b);
                    } else {
                        self.scratch[..words].fill(0);
                        for &d in docs {
                            if self.result[d as usize >> 6] >> (d & 63) & 1 != 0 {
                                set_bit(&mut self.scratch, d);
                            }
                        }
                        std::mem::swap(&mut self.result, &mut self.scratch);
                    }
                }
                Op::Not if any && (at != NONE || !docs.is_empty()) => {
                    if at != NONE {
                        let bits = &self.dense[at as usize * STRIDE..][..words];
                        self.result[..words].iter_mut().zip(bits).for_each(|(r, b)| *r &= !b);
                    } else {
                        for &d in docs {
                            self.result[d as usize >> 6] &= !(1 << (d & 63));
                        }
                    }
                }
                _ => {}
            }
        }
        any
    }

    /// Answers every BOOL query on the chunk.
    fn answer_bools(&mut self, base: u32, words: usize, shared: &Shared, out: &mut Vec<u32>) {
        let q = shared.q;
        self.result.resize(words.max(self.result.len()), 0);
        self.scratch.resize(words.max(self.scratch.len()), 0);
        for &(query, first, count) in &q.bools {
            let ops = &q.bool_ops[first as usize..(first + count) as usize];
            if !self.evaluate(ops, words) {
                continue;
            }
            let (mut matches, mut sum) = (0u64, 0u64);
            let mut quota = 10 - self.emitted[query as usize] as u32;
            for (w, &word) in self.result[..words].iter().enumerate() {
                if word == 0 {
                    continue;
                }
                let n = word.count_ones() as u64;
                matches += n;
                // The sum of a word's bit positions, one popcount per bit of the position.
                const MASKS: [u64; 6] = [
                    0xAAAAAAAAAAAAAAAA,
                    0xCCCCCCCCCCCCCCCC,
                    0xF0F0F0F0F0F0F0F0,
                    0xFF00FF00FF00FF00,
                    0xFFFF0000FFFF0000,
                    0xFFFFFFFF00000000,
                ];
                let positions: u64 = (0..6).map(|k| ((word & MASKS[k]).count_ones() as u64) << k).sum();
                sum += n * (base as u64 + 64 * w as u64) + positions;
                let mut rest = word;
                while quota > 0 && rest != 0 {
                    out.push(query << 16 | (w as u32 * 64 + rest.trailing_zeros()));
                    rest &= rest - 1;
                    quota -= 1;
                }
            }
            if matches > 0 {
                self.emitted[query as usize] = 10 - quota as u8;
                shared.counts[query as usize].fetch_add(matches as u32, Relaxed);
                shared.sums[query as usize].fetch_add(sum, Relaxed);
            }
        }
    }
}

/// Whether the phrase is in the text with its anchor term at `at`, reading outward from there.
unsafe fn phrase_at(q: &Queries, phrase: &Phrase, at: *const u8) -> bool {
    let text = |t: u32| q.terms.text[t as usize];
    let same = |p: *const u8, len: usize, t: u32| {
        let word = text(t);
        word.len() == len && (0..len).all(|i| (*p.add(i)) | ((*p.add(i) >= b'A') as u8 * 0x20) == word[i])
    };
    let mut p = at;
    for &t in phrase.terms[..phrase.anchor_at].iter().rev() {
        // Back over separators to the end of the previous token; TAB or LF means the text ended.
        loop {
            p = p.sub(1);
            if is_token_byte(*p) {
                break;
            }
            if *p == b'\t' || *p == b'\n' {
                return false;
            }
        }
        let end = p.add(1);
        while is_token_byte(*p.sub(1)) {
            p = p.sub(1);
        }
        if !same(p, end as usize - p as usize, t) {
            return false;
        }
    }
    let mut p = at.add(text(phrase.terms[phrase.anchor_at]).len());
    for &t in &phrase.terms[phrase.anchor_at + 1..] {
        while !is_token_byte(*p) {
            if *p == b'\n' {
                return false;
            }
            p = p.add(1);
        }
        let start = p;
        while is_token_byte(*p) {
            p = p.add(1);
        }
        if !same(start, p as usize - start as usize, t) {
            return false;
        }
    }
    true
}

/// First document start at or after `from`.
fn doc_start(input: &[u8], from: usize) -> usize {
    if from == 0 || from >= input.len() {
        return from.min(input.len());
    }
    from + input[from..].iter().position(|&b| b == b'\n').map_or(input.len() - from, |i| i + 1)
}

fn push_number(out: &mut Vec<u8>, mut v: u64) {
    let mut buf = [0u8; 20];
    let mut i = 20;
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    out.extend_from_slice(&buf[i..]);
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
    let file = File::open(format!("{data}/docs.txt")).unwrap();
    let input = map_file(format!("{data}/docs.txt"));
    let mut q = parse_queries(map_file(format!("{data}/queries.txt")));
    let sample_end = doc_start(input, SAMPLE_BYTES);
    let frequency = sample_frequencies(&q, &input[..sample_end]);
    let plan = make_plan(&mut q, &frequency);

    let threads = thread::available_parallelism().map_or(12, |n| n.get()).min(12);
    let chunks = input.len().div_ceil(CHUNK_BYTES);
    let shared = Shared {
        q: &q,
        plan: &plan,
        counts: (0..q.total).map(|_| AtomicU32::new(0)).collect(),
        sums: (0..q.total).map(|_| AtomicU64::new(0)).collect(),
    };
    let firsts: Vec<Mutex<ChunkFirsts>> =
        (0..chunks).map(|_| Mutex::new(ChunkFirsts { base: 0, entries: Vec::new() })).collect();
    let claim = AtomicUsize::new(0);
    let hit_lists: Vec<Vec<(u32, u32)>> = thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut worker = Worker::new(&q, &plan);
                    loop {
                        let i = claim.fetch_add(1, Relaxed);
                        if i >= chunks {
                            return std::mem::take(&mut worker.hits);
                        }
                        let (from, to) = (doc_start(input, i * CHUNK_BYTES), doc_start(input, (i + 1) * CHUNK_BYTES));
                        let (base, docs) = worker.scan(&file, from, to, &shared);
                        let words = docs.div_ceil(64).max(1);
                        worker.invert(words, plan.frequent);
                        let mut slot = firsts[i].lock().unwrap();
                        slot.base = base;
                        worker.answer_bools(base, words, &shared, &mut slot.entries);
                    }
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // Chunks in ascending order give each query its smallest ids first.
    let mut first_ten = vec![[0u32; 10]; q.total];
    let mut filled = vec![0u8; q.total];
    for chunk in &firsts {
        let chunk = chunk.lock().unwrap();
        for &entry in &chunk.entries {
            let query = (entry >> 16) as usize;
            if filled[query] < 10 {
                first_ten[query][filled[query] as usize] = chunk.base + (entry & 0xFFFF);
                filled[query] += 1;
            }
        }
    }
    let mut hits: Vec<(u32, u32)> = hit_lists.into_iter().flatten().collect();
    hits.sort_unstable();
    hits.dedup();
    let (mut counts, mut sums) = (vec![0u64; q.total], vec![0u64; q.total]);
    for (query, c) in shared.counts.iter().enumerate() {
        counts[query] = c.load(Relaxed) as u64;
        sums[query] = shared.sums[query].load(Relaxed);
    }
    for &(query, doc) in &hits {
        let query = query as usize;
        counts[query] += 1;
        sums[query] += doc as u64;
        if filled[query] < 10 {
            first_ten[query][filled[query] as usize] = doc;
            filled[query] += 1;
        }
    }
    let mut text = Vec::with_capacity(q.total * 32);
    for query in 0..q.total {
        push_number(&mut text, counts[query]);
        text.push(b' ');
        push_number(&mut text, sums[query]);
        for &doc in &first_ten[query][..filled[query] as usize] {
            text.push(b' ');
            push_number(&mut text, doc as u64);
        }
        text.push(b'\n');
    }
    File::create(format!("{out}/results.txt")).unwrap().write_all(&text).unwrap();
    // Skip unmapping and destructors: the process is done and the clock is running.
    std::process::exit(0);
}
