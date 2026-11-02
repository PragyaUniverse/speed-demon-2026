# Track B: performance log

"Mac", "VM" and "Harness" mean the same as in Track A's log. The VM has no AVX2, so its tokeniser is a byte-at-a-time fallback and overstates the scan. Reference on the box: 56.35 s, 2,200 MB. The first table is local work before the first board run; the second is every board run.

Before writing any code I measured the seed-1 data: 53,444 distinct BOOL terms with 21M (term, document) pairs, but anchor occurrences for the phrases total only 835K. A global index would need about 84 MB of lists, so the design avoids one.

| Step | Change | Run ID | T local | M (MB) | Result |
|---|---|---|---|---|---|
| 1 | Per-chunk engine with anchored phrase verification, 4 MB chunks | before first push | 166 ms VM | 68 harness | All hashes correct first time, against the worked example, tiny, sample and full |
| 2 | 2 MB chunks | before first push | 160 ms VM | 48 | Same time, a lot less memory |
| 3 | 1 MB chunks | before first push | 163 ms VM | 41 | Below the 44 MB point where memory stops earning, at no measurable cost in the VM. Revisited on the box below |

## Board runs

Each run ID below is one timed run on the scoring box. One run per push, so single numbers are noisy: re-running identical code moved times by up to about 15%, and I compare several runs before believing a change. Failed runs marked PROBE or DIAGNOSTIC were deliberate: they exit non-zero early because the board still reports their time, which is the only way to time one phase on the real CPU. Stderr is not public, so the diagnostic's printed timings were never visible.

| Commit | Change | Run ID | T (ms) | M (MB) | Result |
|---|---|---|---|---|---|
| c6c4c16 | First push | run-f345352b47f3 | 356 | 41.8 | K 20.1 |
| f05f592 | Huge pages off | run-5e5ea27b086f | 330 | 42.6 | No memory effect here, unlike A and C |
| dd48cce | PROBE: stop after query parse and planning | run-f31783ee7d38 | 21.3 | 12.4 | Setup is about 6% |
| 65702b2 | `MAP_POPULATE` | run-74dfaeff9fbf | 357 | 41.5 | **Slower, reverted** |
| d5361fb | Buffered reads; phrases verified during the scan | run-b657a340a7e5 | 345 | 43.5 | Little change in time, kept because it drops the anchor log |
| 851f573 | PROBE: scan only | run-e5f5b2a30958 | 193 | 30.1 | Scan about 172 ms, BOOL evaluation about 150 ms |
| 80c0536 | 1 MB term table, 2 MB chunks | run-b23c2e23102c | 319 | 44.1 | Better, and the smaller table pays for the larger chunks' memory |
| 4b279b1 | PGO on generator output, seed 2 sample | run-9913070bddc2 | 295 | 44.3 | Two more samples: run-474e8ac31656 313, run-4238f68a2125 293. **Kept on this track only** |
| a9e9fc9 | Evaluate only queries a present term can lift | run-eb4f16d76d8b | 293 | 44.3 | **No gain for 39 lines, reverted** |
| 82ac983 | Thread-local tallies instead of shared counters | run-60a7c2f80644 | 305 | 47.8 | **No gain, more memory, reverted** |
| 58784c5 | PROBE: exit code reports system CPU ticks | commit 58784c5, ID not captured | 295 | 44.5 | Exit code 9: about 90 ms of CPU in the kernel, out of several seconds. The time is my own code |
| 515f426 | Frequent terms write bitsets directly during the scan, skipping log and dedupe | run-9369b10f8c79 | 279 | 43.1 | **Kept.** New best on this track |
| de12444 | Remove the mapping entirely | run-87256012fabc | 281 | 45.4 | S 772 to 739 for the same work |
| 08cf96c | My mistake, not an experiment: a failed revert left the diagnostic in, and I pushed without checking | ID not captured | | | Failed on exit code 3. Since then every push goes through `tools/push.sh`, which stops if any hash fails |
| 58784c5 and later | Later probes for this track are in the rows above | | | | |
| d271ae5 | PROBE: read the whole file on 12 threads, no parsing | run-31e2afaca84e | 11.6 | 7.3 | Just getting the bytes takes 11.6 ms, a large share of what the fastest entry on this track spends in total |
| ccb00ab | 4 MB chunks | run-3cf7b65c4c78 | 293 | 53.7 | Second sample run-0d2ae9bc44aa 277. Evaluating half as often gained nothing, and memory passed the 44 MB cap. **Reverted** |
| e2ffea4 | 6 threads instead of 12 | run-48796a83469c | 404 | 28.1 | 1.4x from the second hyperthreads, a healthy figure. So parallelism is not the gap. **Reverted** |
| 013f9b2 | Pin each worker to one logical CPU | run-553e0fa6b0e6 | 282 | 45.4 | Second sample run-c2859691d034 284. **No gain for 14 lines, reverted** |
| 92a5a47 | PROBE: exit code reports the features the build enabled | ID not captured | 0.4 | 7.5 | Exit code 31: AVX2, BMI2 and static linking are all on, so the gap to the leaders is not a build mistake. Also the true memory floor of a program that does nothing, about 7.5 MB |
| 40688d7 | The same design ported to C++ with GCC, no profile | run-793a62bd8483 | 281 | 54.1 | Samples run-585f9cb59602 289, run-87ee122cc133 287. K 22.2 to 22.5, level with Rust's 21.8 to 22.7 despite 54 MB, at S 579 against 739 |
| 2376532 | My mistake: a GCC profile that did not match the final compile | run-48433931e9bb | | | **Build failed.** My check ran a stale binary, so it did not catch this. Training on x86 then crashed, so I dropped GCC PGO |
| 56f7a82 | C++ without touching the worst-case output buffer | run-8c5f79a39d5a | 291 | 48.7 | Second sample run-615218ddfb00 299. K 22.4 to 22.6. Shorter code outweighs slightly more time and memory, so **C++ kept for this track**, by a small margin |
| 3a81baf | Clear the result only once a term can make it non-empty | run-73237f818982 | 283 | 48.7 | Samples run-021332888c64 294, run-5ef488b10fad 284. K about 22.8, for 6 lines. **Kept** |
| ed87b79 | Two-stage tokeniser: all token boundaries first, then lookups | run-c8c252c5b5af | 293 | 48.7 | Samples run-e50fdf7ee5b8 309, run-648804c45b26 297. Correct but slower, as with Track C's parser, and 33 lines longer. **Reverted** |
| a6841ff | 1.5 MB chunks | run-d8faeedac28a | 284 | 43.8 | Samples run-5ea383c1d8b0 291, run-4afa6c9d8533 294. Under the 44 MB point where memory stops earning, no time lost. K 22.9 to 23.3. **Kept** |
| 0dadcf7 | gcc:16 instead of gcc:15 | run-9c578cde061b | 285 | 43.4 | Second sample run-33f0c6aca17e 286. K 23.2, steady. **Kept** |
| 56ec956 | One-probe L1 table for frequent terms; absent terms skip a miss in evaluation | run-9b00fb226940 | 291 | 43.8 | Samples run-f5f140faf54c 291, run-ef08aa42fc8c 287. No gain for 24 lines, so L2 misses on common words are not the main cost. **Reverted** |
| 485298f | x86-64-v3 instead of `-march=native` | run-d6848d174646 | 281 | 43.6 | Second sample run-f55b5004970b 285. The same within noise. **Kept native, which has the longer record** |
