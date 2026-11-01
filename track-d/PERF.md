# Track D: performance log

"Mac", "VM" and "Harness" mean the same as in Track A's log. Reference on the box: 72.69 s, 893 MB. Run IDs are filled in as board runs happen, and none has been measured yet.

| Step | Change | Run ID | T local | M (MB) | Result |
|---|---|---|---|---|---|
| 1 | Parallel parse, two packed CSRs, bidirectional Dijkstra, 4-ary heap, hash-table search state | pending | 182 ms VM | 120 harness | Correct first time. Phases: 22 ms to build, 155 ms in queries. So queries are the target, against the spec's hint about parsing |
| 2 | Huge pages for the graph arrays | | 178 vs 182 ms VM | | No visible effect in a VM, where the host's page size bounds it. Kept because a bare-metal box should gain. **To be confirmed on the board, then removed if flat** |
| 3 | Relabel nodes into BFS blobs | pending | 110 ms VM | | Queries fell from 155 to 69 ms for 22 ms of relabelling. The largest single win on this track |
| 4 | Blob size 128 or 2048 instead of 512 | | 124 and 112 ms VM | | No real difference, kept 512 |
| 5 | Hash that keeps low id bits, for locality | | 154 vs 110 ms VM | | **Worse, reverted.** Clusters defeat linear probing |
| 6 | Monotone radix queue instead of the 4-ary heap | | 112 vs 110 ms VM | | **No gain, reverted.** Heap sizes are small enough that sifting is cheap |
| 7 | Paged distance store instead of the hash table | pending | 82 ms VM | 151 | Queries fell to about 33 ms: no probing branch to mispredict |
| 8 | Free the old CSR; invert forward into backward with counters instead of buckets | pending | 80 ms VM | 110 | 40 MB less at the peak for the same time |
| 9 | Store distances as `u32`, rerun in `u64` when a search meets a huge weight | pending | 79 ms VM | 94 | Halves the search state. Only a handful of queries rerun |
| 10 | Page of 16, 32, 64 or 256 ids | | 78 to 83 ms VM | 90 to 104 | Kept 32 |

Query statistics that shaped this, from a counting build: 7.1M settled nodes and 7.6M pops, so stale entries are only about 6%.
