# Track D: Traverse

**Approach.** Parsing and CSR building are a small part of the time; the search dominates, and it is limited by memory, not arithmetic. So the design targets layout:

- **Packed edges.** One `u32` per edge: head in 21 bits, weight in 11. Only about 0.07% of edges are heavier, and they carry a marker and live in a sorted side table. Both directions then fit in about 54 MB.
- **Bidirectional Dijkstra.** It settles about 7.1M nodes across all queries, against about 24M for a forward search. A source with no out-edges or a target with no in-edges is answered at once, which removes the only queries that would otherwise scan the whole main body.
- **Relabelling.** Node ids are random, so without this every step misses the cache. Workers grow compact blobs of 512 nodes by BFS, and each blob gets consecutive ids. Most of a search's next nodes are then on lines it already touched. This is a cheap partial substitute for a space-filling order, and it pays for itself several times over.
- **Paged search state.** Distances live in small pages allocated on demand behind a directory, as `u32`. A search that meets a huge weight gives up and reruns with `u64`. There is no per-thread array of 1.7M distances and no hash table with its probing branch.

**Assumptions from the spec.** Fewer than 2^21 nodes, and distances under 2^43, which the generator's weights guarantee and the code checks.

**Dependencies.** None.

**With 24 more hours.** A better global order (blobs laid out by adjacency), and reducing the parse buckets, which set peak memory.
