# Track B: Index

**Approach.** The queries are known before the corpus is read, so there is no global index. The corpus is cut into 2 MB chunks of about 2,000 documents and read through 256 KB buffers, and a worker handles one chunk from start to finish:

1. Tokenise with AVX2: classify 64 bytes at a time into a token mask and walk its edges, so a token boundary costs a bit operation rather than a mispredicted per-byte branch. A token is looked up by a hash in a table of one u64 per slot, 1 MB in all so it stays in a core's L2, where a match means 63 hash bits agree (about 5e-9 chance of any false match in a run).
2. Invert just that chunk: a counting sort gives each term a list of document offsets, and a term frequent enough gets a bitset.
3. Answer every BOOL query on the chunk, strictly left to right, over bitsets of a few dozen words.
4. Verify phrases. Each phrase has one anchor, its rarest term according to a quick sample of the corpus. At each occurrence of an anchor, during the scan, the phrase is checked by reading the text outward from it. A term's rarity only affects speed, never correctness.

Results are combined per query. Chunks are concatenated in order for the ten smallest ids. Memory is bounded by the chunk, not the corpus.

**Build.** `pgo.profdata` is a profile from `tools/pgo.sh`, trained on public generator output at seed 2. It is about 5 to 10% on this track and nothing on the other three, so only this one uses it.

**Dependencies.** None.

**With 24 more hours.** Parallelise query parsing and final merging, and cut the table's cache misses with a small filter in front of it.
