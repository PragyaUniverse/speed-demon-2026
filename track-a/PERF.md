# Track A: performance log

Every number below was measured locally unless a run ID is given. "Mac" is `tools/verify.py` natively on an Apple M4 Pro. "VM" is `tools/vm.sh`, a Linux VM on the same machine, 12 vCPUs, output on tmpfs like the board's. "Harness" is the organisers' `run_submission.py` in that VM (`tools/score.sh`), which also reads the real `memory.peak`. The VM has no AVX2, so its numbers use the slower SWAR `until_space`; the board's will differ. Reference on the box: 32.05 s, 228 MB. The first table is local work before the first board run; the second is every board run.

| Step | Change | Run ID | T local | M (MB) | Result |
|---|---|---|---|---|---|
| 0 | Reference through `check.py` | n/a | 11.93 s Mac | | Confirmed setup and hash |
| 1 | Simple Rust: `HashMap`, full sort, `read_to_string` | before first push | 370 ms Mac | 303 RSS | Correct baseline, 32x the Python on the same machine |
| 2 | mmap, 12 workers, packed per-hour records, shared interner, select instead of sort | before first push | 20.5 ms VM | 33.3 harness | About 18x faster. Most of the win is parallelism plus never allocating per line |
| 3 | Try `MAP_POPULATE` | | 23.7 vs 20.5 ms VM | | **Slower, reverted.** Workers fault file pages about as cheaply in parallel, and populate serialises them first |
| 4 | Try writing through a shared mapping of the output, to avoid the inode lock in `pwrite` | | 35 vs 20 ms VM, 26 to 91 ms spread | 25 | **Much slower and erratic, reverted.** Page faults on a shared tmpfs mapping contended badly in the VM. Worth retrying on the box only if a board run shows the serial write is a real cost |
| 5 | Free each hour's buffers as it finishes, with `M_MMAP_THRESHOLD` so frees really unmap | | 21 vs 17 ms in the output phase | 24 | Memory down, time up: each `munmap` is a TLB shootdown across all 12 threads, about 120 of them. **Replaced by step 6** |
| 6 | Segment arenas: chained blocks instead of per-hour `Vec`s, one `madvise` per segment when its last hour is done, 24 calls in total | before first push | 17.2 ms VM | 21.6 harness | Faster **and** smaller than step 2: no reallocation copying and almost no shootdowns. Works because the log is roughly time-ordered, so a segment's hours finish together |

Things kept from the measurements: two-digit division in the number formatter, and `x86-64-v3` with static linking in `.cargo/config.toml`. The static binary skips the dynamic loader, whose cost is not negligible when the whole run takes milliseconds. Its benefit is unmeasured so far, because the local VM is arm64.

## Board runs

Each run ID below is one timed run on the scoring box. One run per push, so single numbers are noisy: re-running identical code moved times by up to about 15%, and I compare several runs before believing a change. Failed runs marked PROBE or DIAGNOSTIC were deliberate: they exit non-zero early because the board still reports their time, which is the only way to time one phase on the real CPU. Stderr is not public, so the diagnostic's printed timings were never visible.

| Commit | Change | Run ID | T (ms) | M (MB) | Result |
|---|---|---|---|---|---|
| c6c4c16 | First push, the design above | run-18f350ef56c8 | 74.6 | 31.7 | K 22.7. T looks like an outlier: identical logic took 31.1 in the next run |
| 429ac9d | DIAGNOSTIC | run-84a5247dcdc5 | 31.1 | 32.3 | Failed on purpose. M is 10 MB above the local harness, near multiples of 2 MB |
| f05f592 | `prctl(PR_SET_THP_DISABLE)` | run-de7709108c70 | 36.0 | 23.3 | K 34.6. The box backs small thread arenas with huge pages, which my VM does not. M down 9 MB |
| dd48cce | PROBE: stop after parsing | run-a0801af172df | 21.7 | 16.8 | Parsing is about two thirds of the run |
| 65702b2 | `MAP_POPULATE` | run-f47696ce3abd | 38.8 | 23.3 | **Slower, reverted.** Same result as in the VM |
| b0c32a5 | Read into 256 KB buffers instead of parsing the mapping | run-15d0f94e3870 | 27.8 | 23.4 | **Kept.** No page faults or unmapping, and the bytes are in L2 when parsed. Later runs of this code: run-e5e4a15297d6 28.4, run-906cc5a3cb21 29.0, run-15876c3b1558 27.8 |
| 80c0536 | No Track A change | run-a8c1f20a6405 | 71.7 | 22.8 | A second outlier on identical code, so the first track of a batch is sometimes hit |
| 4b279b1 | PGO on generator output | run-94e554564b29 | 30.1 | 22.9 | Two more samples: run-2ee4e4c88ee5 28.1, run-f497a41776f6 29.0. **No gain over 28, removed** |
| 82ac983 | Output through a shared mapping, retried on the box | run-cc47fdbb8a0e | 77.4 | 22.9 | **Slower, reverted.** S 483. The VM's result in step 4 was right |
| 58784c5 | PROBE: exit code reports the thread count | commit 58784c5, ID not captured | 34.1 | 23.8 | Exit code 12: all threads present, so parallelism is not the gap to the leaders |
| de12444 | Remove the mapping entirely; endpoint names are small heap copies | run-a52f07941687 | 32.7 | 23.4 | S 459 to 439 for the same work |
| b6d9209 | 128 KB reads | run-299c5c6d3e3f | 66.6 | 22.9 | An outlier, and only 0.5 MB saved. **Reverted** |
