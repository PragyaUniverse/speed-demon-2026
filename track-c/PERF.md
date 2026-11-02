# Track C: performance log

"Mac", "VM" and "Harness" mean the same as in Track A's log: local measurements on an M4 Pro, with no AVX2 in the VM. Reference on the box: 14.44 s, 367 MB. The first table is local work before the first board run; the second is every board run.

| Step | Change | Run ID | T local | M (MB) | Result |
|---|---|---|---|---|---|
| 1 | First correct parser: parallel chunks, flat region table loaded through a `BTreeMap`, shared atomic name bits | before first push | 45 ms harness | 10.2 harness | All hashes correct, including the spec's worked example. The sample took 30 ms on a tenth of the data, a clue that something was a fixed cost |
| 2 | Region load with a direct array keyed by the country code | before first push | 35.8 ms VM, region load 25 to 0.8 ms | | The `BTreeMap` was costing a quarter of a run. Found only because the sample's time did not scale |
| 3 | Measure parts by deleting them: no bit matrix 23.9 ms, also no name table 23.4 ms | | | | The name table is cheap, about 1 ms. The bit matrix costs about 11 ms: locked read-modify-writes on lines bouncing between cores |
| 4 | Batch bit updates under a lock per cache line | | 33.9 vs 35.8 ms VM | 10.5 | **Barely helped, replaced.** No locked instruction per record any more, but the lines still migrate between cores |
| 5 | Each matrix line has one owning worker; pairs travel to the owner in batches | before first push | 31.2 ms VM | 11.2 | The line stays in one L2. About 3 ms better than step 4, with the batches themselves as the remaining traffic |

Correctness checks beyond the hashes: differential runs against the Python reference on seeds 2, 3 and 4 at tiny and sample scale (`tools/seeds.py`), and an x86-64 build under Rosetta on all three seed-1 scales, since that is the only local way to run the AVX2 path.

## Board runs

Each run ID below is one timed run on the scoring box. One run per push, so single numbers are noisy: re-running identical code moved times by up to about 15%, and I compare several runs before believing a change. Failed runs marked PROBE or DIAGNOSTIC were deliberate: they exit non-zero early because the board still reports their time, which is the only way to time one phase on the real CPU. Stderr is not public, so the diagnostic's printed timings were never visible.

| Commit | Change | Run ID | T (ms) | M (MB) | Result |
|---|---|---|---|---|---|
| c6c4c16 | First push | run-87843a9faf84 | 42.4 | 33.6 | K 24.1. M is three times the local 11 MB, and is exactly 32 MiB |
| f05f592 | `prctl(PR_SET_THP_DISABLE)` | run-480db1bea26f | 41.2 | 9.6 | K 31.3. The clue that led to this: 12 thread arenas at 2 MB each |
| dd48cce | PROBE: parse without name tracking | run-15d1525bda44 | 28.0 | 7.4 | Name tracking is about 13 ms of 41 |
| 65702b2 | `MAP_POPULATE` | run-5da8cd0d7782 | 48.9 | 10.9 | **Slower, reverted** |
| d5361fb | Buffered reads, as in Track A | run-8b22f8aa0451 | 40.4 | 13.5 | About 1 ms for 3 MB more: a net loss on K. **Reverted** |
| 4b279b1 | PGO on generator output | run-3bfbd10e33df | 51.0 | 11.7 | Two more samples: run-7f8fceeab9e9 46.4, run-001cdb519679 42.2. **No gain, removed** |
| a9e9fc9 | No Track C change, PGO removed | run-c3a338b69070 | 41.2 | 10.0 | Same as before PGO |
| 82ac983 | Prefetch ring in front of the name table | run-79fb8c5eb986 | 41.3 | 10.5 | **No gain for 35 lines, reverted.** So the 13 ms of name tracking is not mainly the table's misses |
| 58784c5 | PROBE: exit code reports user CPU ticks | commit 58784c5, ID not captured | 41.4 | 10.3 | Exit code 37: about 370 ms of CPU, or 9 of 12 threads busy throughout. Parallelism is fine; the cost is per record |
| b6d9209 | Name batches of 512 | run-802e178c7c4b | 40.5 | 11.6 | About 1 ms for 1.4 MB. **Reverted** |
| 08cf96c | My mistake, not an experiment: a failed revert left the diagnostic in, and I pushed without checking | ID not captured | | | Failed on exit code 3. Since then every push goes through `tools/push.sh`, which stops if any hash fails |
| 58784c5 and later | Later probes for this track are in the rows above | | | | |
| b9545ea | 64-byte quote search, loop-free hash for short names | run-779fd07fbc9d | 42.5 | 9.9 | Second sample run-b0c5f801f0ee 41.2. **No gain, reverted** |
| d271ae5 | PROBE: touch every cache line of the mapping, no parsing | run-22cb43d6855b | 11.3 | 8.2 | Input alone costs 11.3 ms, leaving about 30 ms of my compute |
| ccb00ab | Two-stage parser: bitmask separators, then records from positions, as in simdjson | run-bb071a46549d | 50.6 | 12.4 | Second sample run-9451af89c7c6 46.0. Correct first time, but slower by about 15%, 70 lines longer and 2.5 MB bigger. The most interesting failure here: I expected branch mispredictions to be the cost, and this removed nearly all of them. **Reverted** |
| e2ffea4 | 6 threads instead of 12 | run-553e6c388abb | 53.7 | 10.2 | 1.3x from the second hyperthreads. About 240 cycles per record on a whole core. **Reverted** |
| 013f9b2 | Pin each worker to one logical CPU | run-74b5c19678d9 | 40.9 | 9.9 | Second sample run-22bf09839ea6 41.9. **No gain for 12 lines, reverted** |
| 92a5a47 | PROBE: exit code reports the features the build enabled | ID not captured | 0.4 | 7.5 | Exit code 31: AVX2, BMI2 and static linking are all on, so the gap to the leaders is not a build mistake. Also the true memory floor of a program that does nothing, about 7.5 MB |
| | Callgrind on seed-1 sample, one thread, arm64 (`tools/profile.sh`) | n/a | | | About 300 instructions per record with a flat profile: no single line above 6%. There is no hot spot to fix, so a real gain needs a different design |
| f3c75b8 | The same design ported to C++ with GCC, -march=native on the box | run-47cb7230f67a | 49.9 | 9.9 | Second sample run-cbac58d59584 48.6. Correct on every check and 98 lines shorter (S 354), but about 20% slower: K about 29.5 against 31.3. So the compiler is not the gap to the leaders either. **Reverted to Rust** |
| 3a81baf | Plain atomic name bits instead of owners and batches | run-30b836e83487 | 62.9 | 9.9 | Samples run-2429b2cfbd06 60.9, run-2c6e3a6cd7ed 61.8. 54 lines shorter but 50% slower: K about 25.7 against 31. Coherence traffic is the single most expensive thing measured on this machine, and it proves the owner scheme earns its lines. **Reverted** |
| 0dadcf7 | `target-cpu=native` | run-ba21f8845b87 | 40.2 | 10.3 | Second sample run-4e716951e56f 46.6. No gain. **Reverted** |
