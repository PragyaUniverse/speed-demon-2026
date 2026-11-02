# Track B: Index

**Approach.** The queries are known before the corpus is read, so there is no global index. The corpus is cut into 1.5 MB chunks of about 1,500 documents and read through 256 KB buffers, and a worker handles one chunk from start to finish:

1. Tokenise with AVX2: classify 64 bytes at a time into a token mask and walk its edges, so a token boundary costs a bit operation rather than a mispredicted per-byte branch. A token is looked up by a hash in a table of one u64 per slot, 1 MB in all so it stays in a core's L2, where a match means 63 hash bits agree (about 5e-9 chance of any false match in a run).
   Terms that are frequent in the sample skip that step: their bitset is written directly during the scan.
2. Invert just that chunk: a counting sort gives each term a list of document offsets, and a term frequent enough gets a bitset.
3. Answer every BOOL query on the chunk, strictly left to right, over bitsets of a few dozen words.
4. Verify phrases. Each phrase has one anchor, its rarest term according to a quick sample of the corpus. At each occurrence of an anchor, during the scan, the phrase is checked by reading the text outward from it. A term's rarity only affects speed, never correctness.

Results are combined per query. Chunks are concatenated in order for the ten smallest ids. Memory is bounded by the chunk, not the corpus.

**Language.** C++ with GCC. I built this design in both Rust and C++ and raced them on the board. They tied on speed, and C++ came out about 160 lines shorter, so it has a slightly higher K here; Rust won the other three tracks. The earlier Rust version is in the git history.

**Dependencies.** None.

**With 24 more hours.** Parallelise query parsing and final merging, and find what still separates this from the fastest entries: board probes show the time is per-token and per-query work in my own code, not the kernel.
