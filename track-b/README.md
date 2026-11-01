# Track B: Index

**Approach.** The queries are known before the corpus is read, so there is no global index. The corpus is cut into 1 MB chunks of about 1,000 documents, and a worker handles one chunk from start to finish:

1. Tokenise with AVX2: classify 64 bytes at a time into a token mask and walk its edges, so a token boundary costs a bit operation rather than a mispredicted per-byte branch. A token is looked up by its first and last eight lowercased bytes and its length, which is exact up to 16 bytes and checked byte by byte above that. Only BOOL terms and phrase anchors are in the table's working set.
2. Invert just that chunk: a counting sort gives each term a list of document offsets, and a term frequent enough gets a bitset.
3. Answer every BOOL query on the chunk, strictly left to right, over bitsets of a few dozen words.
4. Verify phrases. Each phrase has one anchor, its rarest term according to a quick sample of the corpus. Occurrences of anchors are recorded, and the phrase is checked by reading the text outward from each one. A term's rarity only affects speed, never correctness.

Results are combined per query. Chunks are concatenated in order for the ten smallest ids. Memory is bounded by the chunk, not the corpus.

**Dependencies.** None.

**With 24 more hours.** Parallelise query parsing and final merging, and cut the table's cache misses with a small filter in front of it.
