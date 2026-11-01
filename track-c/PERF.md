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
