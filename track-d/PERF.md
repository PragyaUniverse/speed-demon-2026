# Track D: performance log

"Mac", "VM" and "Harness" mean the same as in Track A's log. Reference on the box: 72.69 s, 893 MB. The first table is local work before the first board run; the second is every board run.

| Step | Change | Run ID | T local | M (MB) | Result |
|---|---|---|---|---|---|
| 1 | Parallel parse, two packed CSRs, bidirectional Dijkstra, 4-ary heap, hash-table search state | before first push | 182 ms VM | 120 harness | Correct first time. Phases: 22 ms to build, 155 ms in queries. So queries are the target, against the spec's hint about parsing |
| 2 | Huge pages for the graph arrays | | 178 vs 182 ms VM | | No visible effect in a VM, where the host's page size bounds it. Kept because a bare-metal box should gain. Still unconfirmed on the board |
| 3 | Relabel nodes into BFS blobs | before first push | 110 ms VM | | Queries fell from 155 to 69 ms for 22 ms of relabelling. The largest single win on this track |
| 4 | Blob size 128 or 2048 instead of 512 | | 124 and 112 ms VM | | No real difference, kept 512 |
| 5 | Hash that keeps low id bits, for locality | | 154 vs 110 ms VM | | **Worse, reverted.** Clusters defeat linear probing |
| 6 | Monotone radix queue instead of the 4-ary heap | | 112 vs 110 ms VM | | **No gain, reverted.** Heap sizes are small enough that sifting is cheap |
| 7 | Paged distance store instead of the hash table | before first push | 82 ms VM | 151 | Queries fell to about 33 ms: no probing branch to mispredict |
| 8 | Free the old CSR; invert forward into backward with counters instead of buckets | before first push | 80 ms VM | 110 | 40 MB less at the peak for the same time |
| 9 | Store distances as `u32`, rerun in `u64` when a search meets a huge weight | before first push | 79 ms VM | 94 | Halves the search state. Only a handful of queries rerun |
| 10 | Page of 16, 32, 64 or 256 ids | | 78 to 83 ms VM | 90 to 104 | Kept 32 |

Query statistics that shaped this, from a counting build: 7.1M settled nodes and 7.6M pops, so stale entries are only about 6%.

## Board runs

Each run ID below is one timed run on the scoring box. One run per push, so single numbers are noisy: re-running identical code moved times by up to about 15%, and I compare several runs before believing a change. Failed runs marked PROBE or DIAGNOSTIC were deliberate: they exit non-zero early because the board still reports their time, which is the only way to time one phase on the real CPU. Stderr is not public, so the diagnostic's printed timings were never visible.

| Commit | Change | Run ID | T (ms) | M (MB) | Result |
|---|---|---|---|---|---|
| c6c4c16 | First push | run-725210957a9e | 172 | 96.6 | K 21.4 |
| dd48cce | PROBE: stop after the graph is built | run-b0e1170985de | 81.8 | 100.8 | Half the run is building, far more than the VM suggested |
| 851f573 | PROBE: stop after parsing | run-62cd5903124b | 20.0 | 49.8 | Parsing is cheap, so relabelling and CSR building are about 60 ms |
| 65702b2 | `MAP_POPULATE` | run-00f44d455c83 | 171 | 101.0 | **No gain, reverted** |
| d5361fb | Buffered reads | run-a4590b3b0db3 | 164 | 96.0 | Small gain, kept |
| 4b279b1 | PGO on generator output | run-446f11693771 | 178 | 96.6 | Two more samples: run-47c9fd906cf5 181, run-30cb387eec28 211. **Worse, removed** |
| a9e9fc9 | No Track D change, PGO removed | run-80d20711687f | 171 | 92.5 | |
| 82ac983 | Huge pages off; prefetching in relabel | run-624b22c47b65 | 167 | 84.8 | Memory down 12 MB at the same time, which also settles step 2: huge pages did not earn their memory here. Prefetching gave nothing visible for 34 lines. **Prefetching reverted** |
| 58784c5 | PROBE: exit code reports system CPU ticks, with huge pages off | commit 58784c5, ID not captured | 165 | 87.4 | Exit code 19: about 190 ms of CPU in the kernel |
| de12444 | Remove mapping and huge page code | run-2e1dc2514d60 | 163 | 82.9 | S 651 to 594 for the same work |
| b6d9209 | Do not queue a node that cannot beat the best path | run-b5409bb3a712 | 167 | 83.7 | 8% faster in the VM (94 to 87 ms), within noise on the box. Kept, since it costs 3 lines |
| 08cf96c | My mistake, not an experiment: a failed revert left the diagnostic in, and I pushed without checking | ID not captured | | | Failed on exit code 3. Since then every push goes through `tools/push.sh`, which stops if any hash fails |
| 58784c5 and later | Later probes for this track are in the rows above | | | | |
| d271ae5 | PROBE: spawn 12 threads and exit | run-8c55780453ed | 0.9 | 7.8 | Thread start-up is not the problem |
| 92a5a47 | PROBE: exit code reports the features the build enabled | ID not captured | 0.4 | 7.5 | Exit code 31: AVX2, BMI2 and static linking are all on, so the gap to the leaders is not a build mistake. Also the true memory floor of a program that does nothing, about 7.5 MB |
| 40688d7 | The same design ported to C++ with GCC | run-514374bfdf59 | 244 | 92.1 | Samples run-2e83d37f9093 246, run-63de5e0a5345 242. S 454, but K 19.1 against 21 to 23. **Rust kept** |
| ed87b79 | Reverse CSR through owners and batches instead of atomic counters | run-094e35a9ba97 | 165 | 82.7 | Samples run-0f41de76f983 161, run-eeaa66db1ae1 164, then on the next commit run-794d4073caec 176, run-dc80f93f0f5e 173, run-4a23d73b2abc 182 with no code change. Mean about 170, the same as before. The early gain was drift on the box, a warning about comparing runs from different hours. **Reverted**, 31 lines for nothing |
| 0dadcf7 | `target-cpu=native`; parse buckets at 6 bytes an edge | run-635c7ba2fefc | 167 | 82.9 | Second sample run-cdbbf9022ddc 163. Same time and memory, so the buckets do not set the peak. **Reverted** |
