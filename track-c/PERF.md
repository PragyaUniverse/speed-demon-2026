# Track C: performance log

"Mac", "VM" and "Harness" mean the same as in Track A's log: local measurements on an M4 Pro, with no AVX2 in the VM. Reference on the box: 14.44 s, 367 MB. Run IDs are filled in as board runs happen, and none has been measured yet.

| Step | Change | Run ID | T local | M (MB) | Result |
|---|---|---|---|---|---|
| 1 | First correct parser: parallel chunks, flat region table loaded through a `BTreeMap`, shared atomic name bits | pending | 45 ms harness | 10.2 harness | All hashes correct, including the spec's worked example. The sample took 30 ms on a tenth of the data, a clue that something was a fixed cost |
| 2 | Region load with a direct array keyed by the country code | pending | 35.8 ms VM, region load 25 to 0.8 ms | | The `BTreeMap` was costing a quarter of a run. Found only because the sample's time did not scale |
| 3 | Measure parts by deleting them: no bit matrix 23.9 ms, also no name table 23.4 ms | | | | The name table is cheap, about 1 ms. The bit matrix costs about 11 ms: locked read-modify-writes on lines bouncing between cores |
| 4 | Batch bit updates under a lock per cache line | | 33.9 vs 35.8 ms VM | 10.5 | **Barely helped, replaced.** No locked instruction per record any more, but the lines still migrate between cores |
| 5 | Each matrix line has one owning worker; pairs travel to the owner in batches | pending | 31.2 ms VM | 11.2 | The line stays in one L2. About 3 ms better than step 4, with the batches themselves as the remaining traffic |

Correctness checks beyond the hashes: differential runs against the Python reference on seeds 2, 3 and 4 at tiny and sample scale (`tools/seeds.py`), and an x86-64 build under Rosetta on all three seed-1 scales, since that is the only local way to run the AVX2 path.
